//! Each login's last known usage, and each instance's login and model list
//! (R-ROLL-2): what rollover compares accounts by. There is no live vendor
//! query — only what a turn's limits report said, kept where the next turn,
//! of any session or process, can read it.
//!
//! One file, `usage.json` in krowk's home beside `sessions/` (not the XDG
//! runtime directory, which a logout clears). Writers take `flock` on
//! `usage.json.lock`, read the file again, change it and replace it by
//! rename, so several krowk processes write at once without losing each
//! other's reports. A file that cannot be read is an empty one: usage is a
//! hint, never a reason to fail a turn.
//!
//! Usage is keyed by login, not instance name (D18): instances on one login
//! share a record. A Claude Code instance's login is the account it reports
//! (`login_of`); any other instance is its own login.

use crate::protocol::{LimitState, LimitStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// How long a reading of `window` stays worth believing (D17): the window's
/// own length — `five_hour` 5h, `seven_day*` 7d. For one krowk does not know,
/// the time until its reset when first seen (`first`), else 24h.
fn length_ms(window: &str, first: Option<i64>) -> i64 {
    match window {
        "five_hour" => 5 * HOUR_MS,
        w if w.starts_with("seven_day") => 7 * DAY_MS,
        _ => first.filter(|n| *n > 0).unwrap_or(DAY_MS),
    }
}

/// One window of one login, as last reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub status: LimitState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    /// When krowk saw the report, in milliseconds since the Unix epoch.
    pub seen_at_ms: i64,
    /// The time until the reset when the window was first seen: the length
    /// of a window krowk has no rule for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_span_ms: Option<i64>,
}

/// What an instance last reported at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    pub login: String,
    /// The model ids it listed; none when it listed none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub seen_at_ms: i64,
}

/// The file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Data {
    /// login → window → its last report.
    #[serde(default)]
    pub logins: BTreeMap<String, BTreeMap<String, Window>>,
    /// instance → its login and models.
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceRecord>,
}

/// A window as it reads at a given time, staleness applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub window: String,
    pub status: LimitState,
    /// 0 once the window has reset; none when the report gave no figure.
    pub used_percent: Option<f64>,
    pub resets_at_ms: Option<i64>,
    pub seen_at_ms: i64,
}

impl Data {
    /// The readings of `login` that are not stale (D17): a window last seen
    /// longer ago than its length is left out, as never seen. One whose
    /// reset has passed reads as 0% used.
    pub fn readings(&self, login: &str, now_ms: i64) -> Vec<Reading> {
        let Some(windows) = self.logins.get(login) else { return Vec::new() };
        windows
            .iter()
            .filter(|(name, w)| now_ms - w.seen_at_ms <= length_ms(name, w.first_span_ms))
            .map(|(name, w)| {
                let reset = w.resets_at_ms.is_some_and(|r| r <= now_ms);
                Reading { window: name.clone(), status: if reset { LimitState::Allowed } else { w.status }, used_percent: if reset { Some(0.0) } else { w.used_percent }, resets_at_ms: w.resets_at_ms, seen_at_ms: w.seen_at_ms }
            })
            .collect()
    }

    /// The login and models `instance` last reported, if it ever did.
    pub fn instance(&self, instance: &str) -> Option<&InstanceRecord> {
        self.instances.get(instance)
    }

    /// The login `instance` is signed in as: the one it reported, else its
    /// own name.
    pub fn login_of<'a>(&'a self, instance: &'a str) -> &'a str {
        self.instance(instance).map_or(instance, |i| i.login.as_str())
    }
}

/// The store: a path, and nothing held open.
#[derive(Debug, Clone)]
pub struct Usage {
    path: PathBuf,
}

impl Usage {
    pub fn at(path: impl Into<PathBuf>) -> Usage {
        Usage { path: path.into() }
    }

    /// The store in `home` (krowk's home).
    pub fn in_home(home: &Path) -> Usage {
        Usage::at(home.join(krowk_api::home::USAGE))
    }

    /// Everything saved, as it is now.
    pub fn read(&self) -> Data {
        std::fs::read(&self.path).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default()
    }

    /// Saves one limits report for `login`, seen at `now_ms`. A report that
    /// krowk saw before the one already kept for its window is dropped
    /// (D13: the latest seen wins).
    pub fn record_limit(&self, login: &str, limit: &LimitStatus, now_ms: i64) -> Result<(), String> {
        let window = limit.window.clone().unwrap_or_default();
        self.change(|d| {
            let windows = d.logins.entry(login.to_string()).or_default();
            let kept = windows.get(&window);
            if kept.is_some_and(|k| k.seen_at_ms > now_ms) {
                return;
            }
            let first_span_ms = kept.and_then(|k| k.first_span_ms).or_else(|| limit.resets_at_ms.map(|r| r - now_ms).filter(|n| *n > 0));
            windows.insert(window, Window { status: limit.status, used_percent: limit.used_percent, resets_at_ms: limit.resets_at_ms, seen_at_ms: now_ms, first_span_ms });
        })
    }

    /// Saves `instance`'s login and the models it listed. Told every turn,
    /// it writes only when something changed.
    pub fn record_instance(&self, instance: &str, login: &str, models: &[String], now_ms: i64) -> Result<(), String> {
        self.change(|d| {
            if d.instances.get(instance).is_none_or(|r| r.login != login || r.models != models) {
                d.instances.insert(instance.into(), InstanceRecord { login: login.into(), models: models.to_vec(), seen_at_ms: now_ms });
            }
        })
    }

    /// `from` renamed `to` (R-ROLL-3): its login and models go with it. An
    /// instance that is its own login takes its usage along.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.change(|d| {
            let own = d.instances.get(from).is_none_or(|r| r.login == from);
            if let Some(mut r) = d.instances.remove(from) {
                if own {
                    r.login = to.into();
                }
                d.instances.insert(to.into(), r);
            }
            if own && !d.instances.values().any(|r| r.login == from) && let Some(w) = d.logins.remove(from) {
                d.logins.insert(to.into(), w);
            }
        })
    }

    /// `instance` removed: it is never offered again. Its login's usage
    /// stays for another instance on it, and goes with an instance that was
    /// its own login.
    pub fn forget(&self, instance: &str) -> Result<(), String> {
        self.change(|d| {
            d.instances.remove(instance);
            if !d.instances.values().any(|r| r.login == instance) {
                d.logins.remove(instance);
            }
        })
    }

    /// Under the lock: read the file as it is now, change it, replace it.
    fn change(&self, f: impl FnOnce(&mut Data)) -> Result<(), String> {
        let dir = self.path.parent().ok_or_else(|| format!("{} has no directory", self.path.display()))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let _held = lock(&self.path.with_extension("json.lock"))?;
        let mut data = self.read();
        let before = data.clone();
        f(&mut data);
        if data == before && self.path.exists() {
            return Ok(());
        }
        let raw = serde_json::to_vec_pretty(&data).map_err(|e| e.to_string())?;
        replace(&self.path, &raw).map_err(|e| format!("write {}: {e}", self.path.display()))
    }
}

/// `flock` on `path`, held while the guard lives.
fn lock(path: &Path) -> Result<std::fs::File, String> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns; released when
        // the returned file closes.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("lock {}: {}", path.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(f)
}

/// `raw` at `path`, `0600`, by rename from a file of this process's own.
fn replace(path: &Path, raw: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("json.{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let written = o.open(&tmp).and_then(|mut f| f.write_all(raw).and_then(|()| f.sync_data()));
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The login a Claude Code account is: its email, and its organisation when
/// it has one (D18), so a personal and a team login on one address differ.
pub fn claude_login(email: &str, organization: &str) -> Option<String> {
    let (email, organization) = (email.trim(), organization.trim());
    match (email.is_empty(), organization.is_empty()) {
        (true, _) => None,
        (false, true) => Some(email.to_string()),
        (false, false) => Some(format!("{email} ({organization})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(window: &str, used: f64, resets: Option<i64>) -> LimitStatus {
        LimitStatus { status: LimitState::Allowed, window: Some(window.into()), used_percent: Some(used), resets_at_ms: resets }
    }

    fn store(name: &str) -> (Usage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("krowk-usage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Usage::in_home(&dir), dir)
    }

    #[test]
    fn what_was_saved_is_read_back_after_reopening() {
        let (u, dir) = store("reopen");
        u.record_limit("a@x.com", &limit("five_hour", 40.0, Some(9_000_000)), 1_000).unwrap();
        u.record_instance("claude:a", "a@x.com", &["sonnet".into()], 1_000).unwrap();
        let d = Usage::in_home(&dir).read();
        assert_eq!(d.readings("a@x.com", 2_000), vec![Reading { window: "five_hour".into(), status: LimitState::Allowed, used_percent: Some(40.0), resets_at_ms: Some(9_000_000), seen_at_ms: 1_000 }]);
        assert_eq!(d.login_of("claude:a"), "a@x.com");
        assert_eq!(d.instance("claude:a").unwrap().models, ["sonnet"]);
        assert_eq!(d.login_of("codex"), "codex");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_writers_leave_a_valid_file_with_both_their_reports() {
        let (u, dir) = store("writers");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let u = u.clone();
                std::thread::spawn(move || {
                    for n in 0..10 {
                        u.record_limit(&format!("login{t}"), &limit(&format!("w{n}"), 1.0, None), 5).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let d = u.read();
        assert_eq!(d.logins.len(), 8);
        assert!(d.logins.values().all(|w| w.len() == 10));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_report_seen_latest_wins_over_a_late_older_one() {
        let (u, dir) = store("order");
        u.record_limit("a", &limit("five_hour", 70.0, None), 2_000).unwrap();
        u.record_limit("a", &limit("five_hour", 10.0, None), 1_000).unwrap();
        assert_eq!(u.read().readings("a", 3_000)[0].used_percent, Some(70.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_reading_is_unknown_and_a_passed_reset_is_zero() {
        let (u, dir) = store("stale");
        u.record_limit("a", &limit("five_hour", 90.0, Some(HOUR_MS)), 0).unwrap();
        u.record_limit("a", &limit("seven_day", 50.0, Some(100 * DAY_MS)), 0).unwrap();
        u.record_limit("a", &limit("300m", 30.0, Some(2 * HOUR_MS)), 0).unwrap();
        let d = u.read();
        let used = |now: i64, w: &str| d.readings("a", now).into_iter().find(|r| r.window == w).map(|r| r.used_percent);
        // Believed while it is within the window's length; 0% once it has reset.
        assert_eq!(used(HOUR_MS - 1, "five_hour"), Some(Some(90.0)));
        assert_eq!(used(2 * HOUR_MS, "five_hour"), Some(Some(0.0)));
        // Never seen past five hours; the weekly window still reads.
        assert_eq!(used(6 * HOUR_MS, "five_hour"), None);
        assert_eq!(used(6 * HOUR_MS, "seven_day"), Some(Some(50.0)));
        assert_eq!(used(8 * DAY_MS, "seven_day"), None);
        // A window krowk has no rule for lasts as long as it had to its reset when first seen.
        assert_eq!(used(HOUR_MS, "300m"), Some(Some(30.0)));
        assert_eq!(used(2 * HOUR_MS + 1, "300m"), None);
        assert!(d.readings("nobody", 0).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_renamed_instance_keeps_its_login_and_models_and_a_removed_one_is_gone() {
        let (u, dir) = store("rename");
        u.record_instance("claude:a", "a@x.com", &["sonnet".into()], 1).unwrap();
        u.rename("claude:a", "claude:b").unwrap();
        let d = u.read();
        assert!(d.instance("claude:a").is_none());
        assert_eq!((d.login_of("claude:b"), d.instance("claude:b").unwrap().models.clone()), ("a@x.com", vec!["sonnet".to_string()]));
        u.forget("claude:b").unwrap();
        assert!(u.read().instance("claude:b").is_none());
        // An instance that is its own login takes its usage along.
        u.record_limit("codex", &limit("300m", 5.0, None), 1).unwrap();
        u.rename("codex", "codex:team").unwrap();
        assert_eq!(u.read().readings("codex:team", 2).len(), 1);
        assert!(u.read().readings("codex", 2).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
