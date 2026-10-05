//! `krowk upgrade`, and the notice that a newer release exists — said as
//! often as the gap is worth: a major release once and once more a week
//! later, a minor once, a patch never, a security fix daily until it is
//! installed (canon, harness.md → Update notice). Which installer put this binary here decides how: a source build and an
//! npm install are told their own command rather than having files swapped
//! under a manager that believes it owns them.

use super::{Ctx, VERSION};
use crate::output::Format;
use krowk_api::{fail, Error};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RELEASE_API: &str = "https://api.github.com/repos/krowkcom/krowk/releases/latest";
/// The recent releases, for the check: a security fix in one skipped counts
/// as much as one in the newest. A hundred is GitHub's most to a page; a
/// krowk further behind than that is a major release behind regardless.
const RELEASES_API: &str = "https://api.github.com/repos/krowkcom/krowk/releases?per_page=100";
const DAY: i64 = 24 * 3600;
const RELEASE_DOWNLOAD: &str = "https://github.com/krowkcom/krowk/releases/download";

/// A release version: three numbers. `dev`, `0.9.0-12-gabc` and the golden
/// stamp are source builds, and nothing here touches a build git owns.
fn is_release(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn numbers(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
}

fn version_less(a: &str, b: &str) -> bool {
    is_release(a) && is_release(b) && numbers(a) < numbers(b)
}

/// How far behind `current` is. Before 1.0 the middle number is the one
/// that breaks things, so 0.12 → 0.13 is a major release.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Gap {
    Patch,
    Minor,
    Major,
}

fn gap(current: &str, latest: &str) -> Option<Gap> {
    if !version_less(current, latest) {
        return None;
    }
    let (a, b) = (numbers(current), numbers(latest));
    Some(if a[0] != b[0] || (a[0] == 0 && a[1] != b[1]) {
        Gap::Major
    } else if a[1] != b[1] {
        Gap::Minor
    } else {
        Gap::Patch
    })
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build()
        .into()
}

fn latest_version(timeout: Duration) -> Result<String, Error> {
    let mut res = agent(timeout)
        .get(RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| fail("network_unreachable", format!("could not reach {RELEASE_API}: {e}")))?;
    let status = res.status().as_u16();
    if status != 200 {
        return Err(fail("malformed_response", format!("the release API answered HTTP {status} — try again, or see {RELEASE_API}")));
    }
    let body: Value = res
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_json()
        .map_err(|e| fail("malformed_response", format!("the release API answered something unreadable: {e}")))?;
    let tag = body.get("tag_name").and_then(Value::as_str).unwrap_or_default();
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if !is_release(version) {
        return Err(fail("malformed_response", format!("the release API named no readable version (got {tag:?})")));
    }
    Ok(version.to_string())
}

/// The newest release and the ones whose notes have a `### Security`
/// section — the notes are the tag's CHANGELOG.md section, and Keep a
/// Changelog names security fixes so. Drafts and pre-releases are not offered.
fn read_releases(list: &Value) -> Option<(String, Vec<String>)> {
    let mut latest: Option<String> = None;
    let mut security = Vec::new();
    for r in list.as_array()? {
        if r.get("draft").and_then(Value::as_bool).unwrap_or(false) || r.get("prerelease").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let tag = r.get("tag_name").and_then(Value::as_str).unwrap_or_default();
        let v = tag.strip_prefix('v').unwrap_or(tag);
        if !is_release(v) {
            continue;
        }
        let body = r.get("body").and_then(Value::as_str).unwrap_or_default();
        if body.lines().any(|l| l.trim().eq_ignore_ascii_case("### security")) {
            security.push(v.to_string());
        }
        if latest.as_deref().is_none_or(|l| version_less(l, v)) {
            latest = Some(v.to_string());
        }
    }
    Some((latest?, security))
}

fn recent_releases(timeout: Duration) -> Option<(String, Vec<String>)> {
    let mut res = agent(timeout).get(RELEASES_API).header("Accept", "application/vnd.github+json").call().ok()?;
    if res.status().as_u16() != 200 {
        return None;
    }
    read_releases(&res.body_mut().with_config().limit(16 << 20).read_json().ok()?)
}

pub(crate) fn upgrade(ctx: &mut Ctx) -> Result<(), Error> {
    if !is_release(VERSION) {
        return Err(fail(
            "not_upgradable",
            format!("this build came from source (version {VERSION}) — upgrade with `git pull && make install`, or `cargo install --locked --git https://github.com/krowkcom/krowk --features sessions krowk`"),
        ));
    }
    let latest = latest_version(Duration::from_secs(10))?;
    // The newest, for the notice; not when it was checked, since this asked
    // nothing about security fixes and the next check still should. Only
    // ever raised: GitHub's "latest" can be a backport older than another.
    if let Some(path) = state_path(ctx) {
        let mut state = read_state(&path);
        if state.get("latest").and_then(Value::as_str).is_none_or(|l| version_less(l, &latest)) {
            state["latest"] = json!(latest);
            write_state(&path, &state);
        }
    }
    if !version_less(VERSION, &latest) {
        return report(ctx, json!({ "upgraded": false, "version": VERSION, "latest": latest }), &format!("krowk {VERSION} is the latest release"));
    }
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .map_err(|e| fail("not_upgradable", format!("could not find this binary to replace it: {e}")))?;
    if exe.components().any(|c| c.as_os_str() == "node_modules") {
        return Err(fail("not_upgradable", "this krowk was installed by npm, which owns its files — run `npm install -g @krowk/cli@latest`"));
    }
    if cfg!(windows) {
        return Err(fail(
            "not_upgradable",
            format!(
                "krowk does not replace itself on Windows — run `npm install -g @krowk/cli@latest`, or unpack the release archive over this binary: {RELEASE_DOWNLOAD}/v{latest}"
            ),
        ));
    }
    let replaced = self_upgrade(exe.parent().unwrap_or(Path::new(".")), &latest)?;
    let human = format!("upgraded {VERSION} → {latest}\n  {}", replaced.join("\n  "));
    report(ctx, json!({ "upgraded": true, "version": latest, "from": VERSION, "binaries": replaced }), &human)
}

fn report(ctx: &mut Ctx, data: Value, human: &str) -> Result<(), Error> {
    if ctx.format != Format::Human {
        return ctx.emit(&crate::output::encode(&data));
    }
    let _ = writeln!(ctx.io.stdout, "{human}");
    Ok(())
}

/// The platform's archive, named as the release publishes it — the same
/// names the Go build's upgrader asks for, so an install of either upgrades
/// into the other. An upgrade stays on the build it is (R-PKG-3): the lean
/// build, which an installer put in a container or on a CI runner, into
/// `krowk-lean_…`; anything with the session store into the full build,
/// under the name every earlier release used.
fn archive_name(version: &str) -> String {
    let os = if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let name = if cfg!(feature = "sessions") { "krowk" } else { "krowk-lean" };
    format!("{name}_{version}_{os}_{arch}.tar.gz")
}

/// Downloads the archive and its checksum, refuses a mismatch, and swaps each
/// binary in by rename, so a failure midway leaves nothing half-written.
fn self_upgrade(dest: &Path, version: &str) -> Result<Vec<String>, Error> {
    let archive = archive_name(version);
    let base = format!("{RELEASE_DOWNLOAD}/v{version}");
    let sums = String::from_utf8_lossy(&fetch(&format!("{base}/checksums.txt"), 1 << 20)?).into_owned();
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim() == archive)
        .map(|(sum, _)| sum.to_string())
        .ok_or_else(|| fail("malformed_response", format!("checksums.txt on release v{version} does not cover {archive} — nothing was installed")))?;
    let data = fetch(&format!("{base}/{archive}"), 1 << 30)?;
    use sha2::Digest;
    let got: String = sha2::Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        return Err(fail("malformed_response", format!("{archive} does not match its published checksum — nothing was installed; try again")));
    }
    let mut replaced = Vec::new();
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(&data[..]));
    let unreadable = |e: std::io::Error| fail("malformed_response", format!("the release archive is unreadable: {e}"));
    for entry in entries.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let name = entry.path().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        if !entry.header().entry_type().is_file() || !matches!(name.as_str(), "krowk" | "krowk-mcp") {
            continue;
        }
        let target = dest.join(&name);
        write_binary(&mut entry, &target).map_err(|e| {
            fail("not_upgradable", format!("could not write {}: {e} — anything already replaced stays replaced", target.display()))
        })?;
        replaced.push(target.display().to_string());
    }
    if replaced.is_empty() {
        return Err(fail("malformed_response", format!("{archive} holds no krowk binary — nothing was installed")));
    }
    Ok(replaced)
}

fn fetch(url: &str, limit: u64) -> Result<Vec<u8>, Error> {
    let mut res = agent(Duration::from_secs(300)).get(url).call().map_err(|e| fail("network_unreachable", format!("could not reach {url}: {e}")))?;
    let status = res.status().as_u16();
    if status != 200 {
        return Err(fail("malformed_response", format!("{url} answered HTTP {status} — nothing was installed")));
    }
    let mut data = Vec::new();
    res.body_mut()
        .as_reader()
        .take(limit)
        .read_to_end(&mut data)
        .map_err(|e| fail("network_unreachable", format!("the download from {url} broke off: {e}")))?;
    Ok(data)
}

fn write_binary(r: &mut impl Read, dest: &Path) -> std::io::Result<()> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    let prefix = format!(".{}.new-", dest.file_name().unwrap_or_default().to_string_lossy());
    // A fresh, exclusively created name: the directory a binary lives in
    // may be shared, and a predictable name there is one a symlink can wait at.
    let (mut f, tmp) = krowk_api::tempfile::create_file(dir, &prefix, "", 0o755)?;
    let result = (|| {
        std::io::copy(r, &mut f)?;
        drop(f);
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `cache/update-check.json` in krowk's home; none without one. It holds
/// when the releases were last asked about, the newest, the security ones,
/// and the notice already given for the newest (`shown`).
fn state_path(ctx: &Ctx) -> Option<PathBuf> {
    krowk_api::home::dir(ctx.io.env).ok().map(|h| h.join(krowk_api::home::CACHE).join("update-check.json"))
}

fn read_state(path: &Path) -> Value {
    std::fs::read(path).ok().and_then(|d| serde_json::from_slice::<Value>(&d).ok()).filter(Value::is_object).unwrap_or(json!({}))
}

/// By rename: the TUI's refresh runs on a thread the process may end
/// mid-write, and a torn file would lose what was already shown.
fn write_state(path: &Path, state: &Value) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join(format!(".update-check-{}.json", std::process::id()));
    if std::fs::write(&tmp, state.to_string()).and_then(|()| std::fs::rename(&tmp, path)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn at(state: &Value, key: &str) -> Option<jiff::Timestamp> {
    state.get(key).and_then(Value::as_str).and_then(|t| t.parse().ok())
}

fn since(t: Option<jiff::Timestamp>, now: jiff::Timestamp) -> Option<i64> {
    t.map(|t| now.duration_since(t).as_secs())
}

/// Never on a source build, on CI, or when KROWK_NO_UPDATE_CHECK says not to.
fn enabled(ctx: &Ctx) -> bool {
    is_release(VERSION) && !krowk_api::truthy(&ctx.env("CI")) && ctx.env("GITHUB_ACTIONS").is_empty() && !krowk_api::truthy(&ctx.env("KROWK_NO_UPDATE_CHECK"))
}

fn stale(state: &Value, now: jiff::Timestamp) -> bool {
    since(at(state, "checked_at"), now).is_none_or(|s| s >= DAY)
}

/// Asks for the releases and folds them into what the file holds now —
/// read again here, so a notice another krowk marked meanwhile is kept.
fn refresh(path: &Path, timeout: Duration) {
    let found = recent_releases(timeout);
    let mut state = read_state(path);
    if let Some((latest, security)) = found {
        state["latest"] = json!(latest);
        state["security"] = json!(security);
    }
    state["checked_at"] = json!(jiff::Timestamp::now().to_string());
    write_state(path, &state);
}

/// A newer release, as the last check found it.
pub(crate) struct Newer {
    pub latest: String,
    /// A release between this one and `latest` fixes a security issue.
    pub security: bool,
    /// Worth a line now; otherwise it is only there to be looked up.
    pub due: bool,
    /// What saying it is kept against (`line`).
    line: String,
}

/// What a notice is kept against: the release line the gap is to — `0.13`
/// before 1.0, `2` for a major release after, `1.4` for a minor — so a patch
/// on a line already mentioned is not news, and a security fix is its own.
fn line(latest: &str, gap: Gap, security: bool) -> String {
    let n = numbers(latest);
    let line = if gap == Gap::Major && n[0] > 0 { n[0].to_string() } else { format!("{}.{}", n[0], n[1]) };
    if security { format!("{line} security") } else { line }
}

/// What `state` says about `current` at `now`; none when it is current.
fn newer(state: &Value, current: &str, now: jiff::Timestamp) -> Option<Newer> {
    let latest = state.get("latest").and_then(Value::as_str)?.to_string();
    let gap = gap(current, &latest)?;
    let security = state
        .get("security")
        .and_then(Value::as_array)
        .is_some_and(|s| s.iter().filter_map(Value::as_str).any(|v| version_less(current, v) && !version_less(&latest, v)));
    let key = line(&latest, gap, security);
    let shown = state.get("shown").filter(|s| s.get("line").and_then(Value::as_str) == Some(key.as_str()));
    let first = shown.and_then(|s| since(at(s, "first"), now));
    let last = shown.and_then(|s| since(at(s, "last"), now));
    let times = shown.and_then(|s| s.get("times")).and_then(Value::as_u64).unwrap_or(0);
    let due = if security {
        last.is_none_or(|s| s >= DAY)
    } else {
        match gap {
            Gap::Major => shown.is_none() || (times < 2 && first.is_some_and(|s| s >= 7 * DAY)),
            Gap::Minor => shown.is_none(),
            Gap::Patch => false,
        }
    };
    Some(Newer { latest, security, due, line: key })
}

/// Records that the notice for `line` was given at `now`.
fn mark_shown(state: &mut Value, line: &str, now: jiff::Timestamp) {
    let now = json!(now.to_string());
    match state.get_mut("shown").filter(|s| s.get("line").and_then(Value::as_str) == Some(line)) {
        Some(s) => {
            s["times"] = json!(s.get("times").and_then(Value::as_u64).unwrap_or(0) + 1);
            s["last"] = now;
        }
        None => state["shown"] = json!({ "line": line, "first": now, "last": now, "times": 1 }),
    }
}

/// After a command that worked: a line on stderr when a newer release is
/// worth one. The check itself runs at most once a day, waiting up to two
/// seconds, as it always has here.
pub(crate) fn maybe_notify(ctx: &mut Ctx) {
    if !enabled(ctx) {
        return;
    }
    let Some(path) = state_path(ctx) else { return };
    if stale(&read_state(&path), jiff::Timestamp::now()) {
        refresh(&path, Duration::from_secs(2));
    }
    let mut state = read_state(&path);
    let now = jiff::Timestamp::now();
    let Some(n) = newer(&state, VERSION, now).filter(|n| n.due) else { return };
    let what = if n.security { "with a security fix " } else { "" };
    let _ = writeln!(ctx.io.stderr, "krowk {} is available {what}(this is {VERSION}) — run `krowk upgrade`", n.latest);
    mark_shown(&mut state, &n.line, now);
    write_state(&path, &state);
}

/// For the TUI, which waits on no network before its prompt (R-PERF-1): the
/// last check's answer, marked shown when it is due, and a check for the
/// next start run on a thread of its own when that one is a day old.
#[cfg(feature = "harness")]
pub(crate) fn for_tui(ctx: &Ctx) -> Option<Newer> {
    if !enabled(ctx) {
        return None;
    }
    let path = state_path(ctx)?;
    let mut state = read_state(&path);
    let now = jiff::Timestamp::now();
    let n = newer(&state, VERSION, now);
    // Marked before the refresh starts, which reads the file again only
    // once its answer is in.
    if let Some(n) = n.as_ref().filter(|n| n.due) {
        mark_shown(&mut state, &n.line, now);
        write_state(&path, &state);
    }
    if stale(&state, now) {
        std::thread::spawn(move || refresh(&path, Duration::from_secs(10)));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_numbers_are_a_release_and_order_is_numeric() {
        assert!(is_release("1.2.3") && !is_release("dev") && !is_release("0.0.0-golden") && !is_release("0.9.0-12-gabc"));
        assert!(version_less("1.9.0", "1.10.0") && !version_less("1.10.0", "1.9.0") && !version_less("dev", "9.9.9"));
        let build = if cfg!(feature = "sessions") { "krowk" } else { "krowk-lean" };
        assert!(archive_name("1.2.3").starts_with(&format!("{build}_1.2.3_")) && archive_name("1.2.3").ends_with(".tar.gz"));
    }

    #[test]
    fn before_1_0_the_middle_number_is_a_major_release() {
        assert_eq!(gap("0.12.1", "0.13.0"), Some(Gap::Major));
        assert_eq!(gap("0.11.2", "0.12.1"), Some(Gap::Major), "the gap, not the newest release's own size");
        assert_eq!(gap("0.12.0", "0.12.3"), Some(Gap::Patch));
        assert_eq!(gap("1.2.0", "2.0.0"), Some(Gap::Major));
        assert_eq!(gap("1.2.9", "1.3.0"), Some(Gap::Minor));
        assert_eq!(gap("1.3.0", "1.3.0"), None);
        assert_eq!(gap("dev", "1.3.0"), None);
    }

    #[test]
    fn the_releases_name_the_newest_and_the_security_fixes() {
        let list = json!([
            { "tag_name": "v0.14.0-rc1", "prerelease": true, "body": "### Security" },
            { "tag_name": "v0.13.1", "body": "### Fixed\n\n- a thing" },
            { "tag_name": "v0.13.0", "body": "### Added\n\n- x\n\n### Security\n\n- y" },
            { "tag_name": "v0.12.9", "draft": true, "body": "" },
        ]);
        assert_eq!(read_releases(&list), Some(("0.13.1".into(), vec!["0.13.0".into()])));
        assert_eq!(read_releases(&json!([])), None);
    }

    fn ts(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    /// Run `now` through the notice as a start would: said or not, and
    /// marked when said.
    fn start(state: &mut Value, current: &str, now: &str) -> bool {
        let n = newer(state, current, ts(now));
        let due = n.as_ref().is_some_and(|n| n.due);
        if due {
            mark_shown(state, &n.unwrap().line, ts(now));
        }
        due
    }

    #[test]
    fn a_major_release_is_said_once_and_once_more_a_week_later() {
        let mut s = json!({ "latest": "0.13.0" });
        assert!(start(&mut s, "0.12.1", "2026-10-01T09:00:00Z"));
        assert!(!start(&mut s, "0.12.1", "2026-10-01T10:00:00Z"));
        assert!(!start(&mut s, "0.12.1", "2026-10-07T09:00:00Z"));
        assert!(start(&mut s, "0.12.1", "2026-10-08T09:00:00Z"), "the reminder");
        assert!(!start(&mut s, "0.12.1", "2026-10-30T09:00:00Z"), "and then nothing");
        s["latest"] = json!("0.13.1");
        assert!(!start(&mut s, "0.12.1", "2026-10-30T10:00:00Z"), "a patch on a line already said is not news");
        s["latest"] = json!("0.14.0");
        assert!(start(&mut s, "0.12.1", "2026-10-31T09:00:00Z"), "a newer line is news again");
        let mut s = json!({ "latest": "2.0.0" });
        assert!(start(&mut s, "1.4.0", "2026-10-01T09:00:00Z"));
        s["latest"] = json!("2.1.0");
        assert!(!start(&mut s, "1.4.0", "2026-10-02T09:00:00Z"), "after 1.0 a major line is its first number");
    }

    #[test]
    fn a_minor_release_is_said_once_and_a_patch_never() {
        let mut s = json!({ "latest": "1.3.0" });
        assert!(start(&mut s, "1.2.0", "2026-10-01T09:00:00Z"));
        assert!(!start(&mut s, "1.2.0", "2026-10-09T09:00:00Z"));
        s["latest"] = json!("1.3.4");
        assert!(!start(&mut s, "1.2.0", "2026-10-10T09:00:00Z"));
        let mut s = json!({ "latest": "0.12.2" });
        assert!(!start(&mut s, "0.12.1", "2026-10-01T09:00:00Z"));
        let n = newer(&s, "0.12.1", ts("2026-10-01T09:00:00Z")).unwrap();
        assert!(n.latest == "0.12.2" && !n.due, "known, for the details overlay, but not said");
    }

    #[test]
    fn a_security_fix_is_said_daily_even_in_a_patch_and_only_when_skipped() {
        let mut s = json!({ "latest": "0.12.3", "security": ["0.12.2"] });
        assert!(start(&mut s, "0.12.1", "2026-10-01T09:00:00Z"));
        assert!(!start(&mut s, "0.12.1", "2026-10-01T20:00:00Z"));
        assert!(start(&mut s, "0.12.1", "2026-10-02T09:00:00Z"));
        assert!(!start(&mut s, "0.12.2", "2026-10-09T09:00:00Z"), "already has the fix: a patch, so nothing");
        let mut s = json!({ "latest": "0.13.0" });
        assert!(start(&mut s, "0.12.1", "2026-10-01T09:00:00Z"));
        s["latest"] = json!("0.13.1");
        s["security"] = json!(["0.13.1"]);
        assert!(start(&mut s, "0.12.1", "2026-10-01T12:00:00Z"), "a security fix on a line just said is said at once");
    }
}
