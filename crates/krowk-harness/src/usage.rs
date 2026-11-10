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

use crate::protocol::{LimitState, LimitStatus, ModelRef};
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
    /// login → when an `auto` move last brought a session there (D19).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moved: BTreeMap<String, i64>,
}

/// How long a login stays marked "moved here" (D19).
const MOVED_MS: i64 = 5 * 60_000;
/// The headroom below which a login is never picked (D7), and the spread
/// of headroom within which candidates count as tied (D8).
const MIN_HEADROOM: f64 = 10.0;
const NEAR_TIE: f64 = 10.0;

/// What is known of a login's room: its headroom (D6) and the binding
/// window's reset and reading time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Standing {
    pub headroom: f64,
    /// The binding window's reset; none when every window has reset.
    pub reset_ms: Option<i64>,
    pub seen_at_ms: i64,
}

/// A rollover candidate, ranked: its standing when its login was seen.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub model: ModelRef,
    pub standing: Option<Standing>,
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

    /// What `login` has left at `now_ms` (D6): 100 minus the highest use
    /// among its windows that have not reset, a reset one counting as 0.
    /// None when it was never seen, or only stale or without a figure.
    pub fn standing(&self, login: &str, now_ms: i64) -> Option<Standing> {
        let all: Vec<Reading> = self.readings(login, now_ms).into_iter().filter(|r| r.used_percent.is_some()).collect();
        let used = |r: &Reading| r.used_percent.unwrap_or(0.0);
        let reset = |r: &Reading| r.resets_at_ms.unwrap_or(i64::MAX);
        let binding = all.iter().filter(|r| r.resets_at_ms.is_none_or(|t| t > now_ms)).max_by(|a, b| used(a).total_cmp(&used(b)).then(reset(b).cmp(&reset(a))));
        match binding {
            Some(b) => Some(Standing { headroom: (100.0 - used(b)).max(0.0), reset_ms: b.resets_at_ms, seen_at_ms: b.seen_at_ms }),
            None => all.iter().map(|r| r.seen_at_ms).max().map(|seen_at_ms| Standing { headroom: 100.0, reset_ms: None, seen_at_ms }),
        }
    }

    /// Whether a limit is in force on `login`: a reading that says limited
    /// and has not reset.
    pub fn limited(&self, login: &str, now_ms: i64) -> bool {
        self.readings(login, now_ms).iter().any(|r| r.status == LimitState::Limited)
    }

    /// Marks `login` as moved to now (D19).
    pub fn mark_moved(&mut self, login: &str, now_ms: i64) {
        self.moved.insert(login.into(), now_ms);
    }

    /// Whether an `auto` move brought a session to `login` in the last 5 minutes.
    pub fn moved_recently(&self, login: &str, now_ms: i64) -> bool {
        self.moved.get(login).is_some_and(|t| *t <= now_ms && now_ms - t < MOVED_MS)
    }

    /// The candidates a session on `from_login` may move to, best first
    /// (D7, D8, D19). The input order breaks the last ties: it is
    /// `rolloverOrder`'s, then the names'. Eligible: not `from_login`, no
    /// limit in force, headroom of 10 or more when known. Known logins go
    /// first, the near-tie group (within 10 points of the most headroom) by
    /// earliest binding reset — none last — then headroom, the rest by
    /// headroom; never-seen ones follow. A recently moved-to login ranks
    /// after the others in its group.
    pub fn rank(&self, from_login: &str, candidates: &[ModelRef], now_ms: i64) -> Vec<Ranked> {
        let mut known: Vec<(usize, Ranked, Standing, bool)> = Vec::new();
        let mut never: Vec<(usize, Ranked, bool)> = Vec::new();
        for (i, m) in candidates.iter().enumerate() {
            let login = self.login_of(&m.instance);
            if login == from_login || self.limited(login, now_ms) {
                continue;
            }
            let moved = self.moved_recently(login, now_ms);
            match self.standing(login, now_ms) {
                Some(s) if s.headroom < MIN_HEADROOM => {}
                Some(s) => known.push((i, Ranked { model: m.clone(), standing: Some(s) }, s, moved)),
                None => never.push((i, Ranked { model: m.clone(), standing: None }, moved)),
            }
        }
        let top = known.iter().map(|k| k.2.headroom).fold(0.0, f64::max);
        let (mut near, mut rest): (Vec<_>, Vec<_>) = known.into_iter().partition(|k| k.2.headroom >= top - NEAR_TIE);
        near.sort_by(|a, b| a.3.cmp(&b.3).then(a.2.reset_ms.unwrap_or(i64::MAX).cmp(&b.2.reset_ms.unwrap_or(i64::MAX))).then(b.2.headroom.total_cmp(&a.2.headroom)).then(a.0.cmp(&b.0)));
        rest.sort_by(|a, b| a.3.cmp(&b.3).then(b.2.headroom.total_cmp(&a.2.headroom)).then(a.0.cmp(&b.0)));
        never.sort_by_key(|n| (n.2, n.0));
        near.into_iter().chain(rest).map(|k| k.1).chain(never.into_iter().map(|n| n.1)).collect()
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

    /// A store where each `(login, window, used, resets)` was seen at `seen`.
    fn seen(rows: &[(&str, &str, f64, Option<i64>)], seen: i64) -> Data {
        let mut d = Data::default();
        for (login, window, used, resets) in rows {
            d.logins.entry(login.to_string()).or_default().insert(window.to_string(), Window { status: LimitState::Allowed, used_percent: Some(*used), resets_at_ms: *resets, seen_at_ms: seen, first_span_ms: None });
            d.instances.insert(format!("i:{login}"), InstanceRecord { login: login.to_string(), models: Vec::new(), seen_at_ms: seen });
        }
        d
    }

    fn cands(logins: &[&str]) -> Vec<ModelRef> {
        logins.iter().map(|l| ModelRef { instance: format!("i:{l}"), model: "m".into() }).collect()
    }

    fn pick(d: &Data, from: &str, logins: &[&str], now: i64) -> Vec<String> {
        d.rank(from, &cands(logins), now).into_iter().map(|r| r.model.instance.trim_start_matches("i:").to_string()).collect()
    }

    #[test]
    fn headroom_is_what_the_fullest_unreset_window_leaves() {
        let d = seen(&[("a", "five_hour", 30.0, Some(10_000)), ("a", "seven_day", 60.0, Some(20_000)), ("b", "five_hour", 90.0, Some(5_000)), ("b", "seven_day", 20.0, Some(20_000))], 1_000);
        let a = d.standing("a", 2_000).unwrap();
        assert_eq!((a.headroom, a.reset_ms), (40.0, Some(20_000)));
        // b's fullest window has reset: it counts as 0%.
        assert_eq!(d.standing("b", 6_000).unwrap().headroom, 80.0);
        // Every window reset: 100% and no binding reset.
        assert_eq!(d.standing("b", 30_000).map(|s| (s.headroom, s.reset_ms)), Some((100.0, None)));
        assert_eq!(d.standing("nobody", 0), None);
    }

    #[test]
    fn eligibility_needs_headroom_a_free_login_and_a_different_one() {
        let mut d = seen(&[("cur", "five_hour", 10.0, None), ("low", "five_hour", 91.0, None), ("ok", "five_hour", 90.0, None), ("lim", "five_hour", 10.0, None)], 1_000);
        d.logins.get_mut("lim").unwrap().get_mut("five_hour").unwrap().status = LimitState::Limited;
        // A second instance on the current login is no candidate (D18).
        d.instances.insert("i:twin".into(), InstanceRecord { login: "cur".into(), models: Vec::new(), seen_at_ms: 1 });
        let got: Vec<String> = d.rank("cur", &[cands(&["low", "ok", "lim", "cur"]), vec![ModelRef { instance: "i:twin".into(), model: "m".into() }]].concat(), 2_000).into_iter().map(|r| r.model.instance).collect();
        assert_eq!(got, ["i:ok"]);
    }

    #[test]
    fn most_left_first_then_the_earliest_reset_among_near_ties() {
        let d = seen(&[("cur", "five_hour", 99.0, None), ("a", "five_hour", 50.0, Some(9_000)), ("b", "five_hour", 55.0, Some(3_000)), ("c", "five_hour", 60.0, Some(2_000)), ("d", "five_hour", 80.0, Some(1_500)), ("e", "five_hour", 55.0, None)], 1_000);
        // Within 10 of a's 50% left: a, b, c (not d), the earliest reset first, and
        // the reset-less e after those that have one; then d by headroom.
        assert_eq!(pick(&d, "cur", &["a", "b", "c", "d", "e"], 1_400), ["c", "b", "a", "e", "d"]);
        // Equal reset: more headroom, then the input order.
        let t = seen(&[("cur", "five_hour", 99.0, None), ("x", "five_hour", 50.0, Some(9)), ("y", "five_hour", 55.0, Some(9)), ("z", "five_hour", 55.0, Some(9))], 1);
        assert_eq!(pick(&t, "cur", &["z", "y", "x"], 2), ["x", "z", "y"]);
    }

    #[test]
    fn never_seen_and_stale_logins_follow_every_known_one() {
        let d = seen(&[("cur", "five_hour", 99.0, None), ("known", "five_hour", 85.0, None), ("old", "five_hour", 0.0, None)], 1_000);
        // old's reading is past five hours (D17): never seen.
        let now = 1_000 + 6 * HOUR_MS;
        let mut d = d;
        d.logins.get_mut("known").unwrap().get_mut("five_hour").unwrap().seen_at_ms = now - 1;
        assert_eq!(pick(&d, "cur", &["new", "old", "known"], now), ["known", "new", "old"]);
    }

    #[test]
    fn a_login_just_moved_to_ranks_after_the_others_in_its_group() {
        let mut d = seen(&[("cur", "five_hour", 99.0, None), ("a", "five_hour", 30.0, Some(1_000)), ("b", "five_hour", 35.0, Some(2_000))], 100);
        assert_eq!(pick(&d, "cur", &["a", "b"], 200), ["a", "b"]);
        d.mark_moved("a", 150);
        assert_eq!(pick(&d, "cur", &["a", "b"], 200), ["b", "a"]);
        // The mark lapses after five minutes.
        assert_eq!(pick(&d, "cur", &["a", "b"], 150 + MOVED_MS), ["a", "b"]);
    }

    #[test]
    fn the_sequence_from_a_nearly_empty_account_never_goes_back() {
        let d = seen(&[("a", "five_hour", 98.0, Some(13_000)), ("b", "five_hour", 93.0, Some(12_500)), ("c", "five_hour", 40.0, Some(15_000)), ("d", "seven_day", 60.0, Some(90_000)), ("e", "seven_day", 60.0, Some(90_000))], 100);
        let first = pick(&d, "a", &["b", "c", "d", "e"], 200);
        assert_eq!(first[0], "c");
        assert!(!first.contains(&"b".to_string()), "7% left is under the floor: {first:?}");
        // After C, A is under the floor while its window is unreset.
        assert!(!pick(&d, "c", &["a", "b", "d", "e"], 200).contains(&"a".to_string()));
    }
}
