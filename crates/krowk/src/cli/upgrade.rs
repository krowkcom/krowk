//! `krowk upgrade`, and the once-a-day notice that a newer release exists.
//! Which installer put this binary here decides how: a source build and an
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
const RELEASE_DOWNLOAD: &str = "https://github.com/krowkcom/krowk/releases/download";

/// A release version: three numbers. `dev`, `0.9.0-12-gabc` and the golden
/// stamp are source builds, and nothing here touches a build git owns.
fn is_release(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn version_less(a: &str, b: &str) -> bool {
    if !is_release(a) || !is_release(b) {
        return false;
    }
    let n = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    n(a) < n(b)
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

pub(crate) fn upgrade(ctx: &mut Ctx) -> Result<(), Error> {
    if !is_release(VERSION) {
        return Err(fail(
            "not_upgradable",
            format!("this build came from source (version {VERSION}) — upgrade with `git pull && make install`, or `cargo install --locked --git https://github.com/krowkcom/krowk --features sessions krowk`"),
        ));
    }
    let latest = latest_version(Duration::from_secs(10))?;
    write_state(ctx, &json!({ "checked_at": jiff::Timestamp::now().to_string(), "latest": latest }));
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

fn state_path(_ctx: &Ctx) -> PathBuf {
    // krowk's config directory, never a relative one (`creds::config_dir`).
    krowk_api::creds::config_dir().join("update-check.json")
}

fn write_state(ctx: &Ctx, state: &Value) {
    let path = state_path(ctx);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, state.to_string());
}

/// After a command that worked, at most once a day, never on a source build,
/// on CI or when KROWK_NO_UPDATE_CHECK says not to: a line on stderr naming
/// a newer release.
pub(crate) fn maybe_notify(ctx: &mut Ctx) {
    if !is_release(VERSION) || krowk_api::truthy(&ctx.env("CI")) || !ctx.env("GITHUB_ACTIONS").is_empty() || krowk_api::truthy(&ctx.env("KROWK_NO_UPDATE_CHECK")) {
        return;
    }
    let path = state_path(ctx);
    let mut state: Value = std::fs::read(&path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or(json!({}));
    let checked = state.get("checked_at").and_then(Value::as_str).and_then(|t| t.parse::<jiff::Timestamp>().ok());
    let stale = checked.is_none_or(|t| jiff::Timestamp::now().duration_since(t).as_secs() >= 24 * 3600);
    if stale {
        if let Ok(latest) = latest_version(Duration::from_secs(2)) {
            state["latest"] = json!(latest);
        }
        state["checked_at"] = json!(jiff::Timestamp::now().to_string());
        write_state(ctx, &state);
    }
    if let Some(latest) = state.get("latest").and_then(Value::as_str).filter(|l| version_less(VERSION, l)) {
        let _ = writeln!(ctx.io.stderr, "krowk {latest} is available (this is {VERSION}) — run `krowk upgrade`");
    }
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
}
