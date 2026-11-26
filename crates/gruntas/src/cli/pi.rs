//! The carried pi extension: npm/pi bundled into one file
//! (npm/pi/bundle.mjs), embedded here as the Claude plugin is, with the
//! gruntas skill. `gruntas pi extension` (plumbing, out of the main help)
//! writes both into `<gruntas home>/pi` and says where, for the
//! `@gruntas/pi` loader; `gruntas run pi` loads the same files.
//!
//! The bundle leaves out the Claude Agent SDK, which gruntas does not
//! distribute: writing the extension also installs the exact version
//! npm/pi/sdk pins from npm beside it, once (`npm ci`, scripts off).

use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Error};
use serde_json::json;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The bundled extension, as npm/pi/bundle.mjs last built it.
const EXTENSION: &str = include_str!("../../../../npm/pi/carried/gruntas.js");
/// The gruntas skill, the installer's `~/.agents/skills/gruntas` at this version.
const SKILL: &str = include_str!("../../../../skills/gruntas/SKILL.md");
/// What the bundle reports as its version until it is written (bundle.mjs's VERSION_TOKEN).
const VERSION_TOKEN: &str = "0.0.0-gruntas-version";
/// The Claude Agent SDK's pin, a package.json and its lockfile with the
/// tarball's integrity: the bundle imports the SDK from `node_modules` beside it.
const SDK_PACKAGE: &str = include_str!("../../../../npm/pi/sdk/package.json");
const SDK_LOCK: &str = include_str!("../../../../npm/pi/sdk/package-lock.json");
const SDK: &str = "@anthropic-ai/claude-agent-sdk";

/// The carried files, written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Carried {
    /// The extension, for `pi -e`.
    pub extension: PathBuf,
    /// The skill's directory, for `pi --skill`.
    pub skill: PathBuf,
    /// What this call wrote: none when everything was already current.
    pub written: Vec<PathBuf>,
}

/// Where the carried files live: `<gruntas home>/pi`, beside the Claude plugin.
pub(crate) fn dir(env: &dyn Fn(&str) -> String) -> Option<PathBuf> {
    Some(gruntas_harness::launch::plugin_dir(env)?.parent()?.join("pi"))
}

/// Writes the carried files into `dir` and makes sure the pinned Agent SDK is
/// beside them. Only the files fail it: without the SDK (no npm, no network)
/// the hooks, MCP tools and skill still work, and this says on stderr how to
/// get pi's `gruntas/` models working.
pub(crate) fn install(dir: &Path) -> std::io::Result<Carried> {
    let mut carried = write(dir)?;
    // The provider is off on Windows (npm/pi/src/gruntas.ts), so nothing there needs the SDK.
    if !cfg!(windows) {
        carried.written.extend(ensure_sdk(dir, &[OsStr::new("npm")]));
    }
    Ok(carried)
}

/// Writes the extension (stamped with this gruntas's version) and the skill
/// into `dir`: only a file whose content differs, through a rename, so a pi
/// loading it never reads half a file and an unchanged one is left alone.
fn write(dir: &Path) -> std::io::Result<Carried> {
    let extension = dir.join("gruntas.js");
    let skill = dir.join("skills").join("gruntas");
    let mut written = Vec::new();
    // The token as the JS string literal it is, replaced by another literal.
    let stamped = EXTENSION.replace(&format!("\"{VERSION_TOKEN}\""), &serde_json::Value::from(super::VERSION).to_string());
    for (file, content) in [(extension.clone(), stamped), (skill.join("SKILL.md"), SKILL.to_string())] {
        if std::fs::read(&file).is_ok_and(|had| had == content.as_bytes()) {
            continue;
        }
        let parent = file.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".{}.{}", file.file_name().and_then(|n| n.to_str()).unwrap_or("carried"), std::process::id()));
        if let Err(e) = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, &file)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        written.push(file);
    }
    Ok(Carried { extension, skill, written })
}

/// The SDK version the embedded lockfile pins.
fn sdk_version() -> Option<String> {
    let lock: serde_json::Value = serde_json::from_str(SDK_LOCK).ok()?;
    lock["packages"][format!("node_modules/{SDK}")]["version"].as_str().map(str::to_owned)
}

/// Whether the package at `sdk` is the SDK at `version`.
fn sdk_is(sdk: &Path, version: &str) -> bool {
    let package = std::fs::read(sdk.join("package.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    package.is_some_and(|p| p["name"] == SDK && p["version"] == version)
}

/// Makes sure `dir/node_modules` holds the pinned SDK, fetching it with
/// `npm` (the command and any arguments before npm's own) when it doesn't.
/// Returns the SDK's directory when this call installed it; a failure is
/// said on stderr, never returned.
fn ensure_sdk(dir: &Path, npm: &[&OsStr]) -> Option<PathBuf> {
    let version = sdk_version()?;
    let sdk = dir.join("node_modules").join(SDK);
    if sdk_is(&sdk, &version) {
        return None;
    }
    match fetch_sdk(dir, npm, &version) {
        Ok(()) => Some(sdk),
        Err(why) => {
            eprintln!(
                "gruntas: the Claude Agent SDK could not be fetched: {why}. pi's gruntas hooks, tools and skill work without it; its gruntas/ models need it: install npm (it comes with Node.js), then run `gruntas pi extension`."
            );
            None
        }
    }
}

/// How long the fetch may take before it counts as failed (5 MB; seconds
/// normally): under the 30 s the `@gruntas/pi` loader gives `gruntas pi
/// extension` (npm/pi/src/loader.ts), so pi's start never waits on npm long.
const SDK_FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

/// `npm ci` of the embedded pin in a staging directory inside `dir`, with
/// its own npm cache there, scripts off and the SDK's optional packages (its
/// own Claude Code, which gruntas starts instead) left out; then its
/// `node_modules` takes the place of `dir`'s by renames, the old one moved
/// aside first. npm checks every tarball against the lockfile's integrity,
/// and writes nowhere outside the staging directory, which goes when this
/// returns.
fn fetch_sdk(dir: &Path, npm: &[&OsStr], version: &str) -> Result<(), String> {
    let stage = dir.join(format!(".sdk.{}", std::process::id()));
    let result = (|| {
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage)
            .and_then(|()| std::fs::write(stage.join("package.json"), SDK_PACKAGE))
            .and_then(|()| std::fs::write(stage.join("package-lock.json"), SDK_LOCK))
            .map_err(|e| format!("{} could not be written: {e}", stage.display()))?;
        let (program, before) = npm.split_first().ok_or("no npm to run")?;
        let mut child = Command::new(program)
            .args(before)
            .arg("ci")
            .arg("--prefix")
            .arg(&stage)
            .arg("--cache")
            .arg(stage.join(".npm-cache"))
            .args([
                "--ignore-scripts",
                "--omit=optional",
                "--legacy-peer-deps",
                "--global=false",
                "--audit=false",
                "--fund=false",
                "--update-notifier=false",
                "--fetch-retries=0",
                "--fetch-timeout=20000",
                "--loglevel=error",
            ])
            .current_dir(&stage)
            .stdin(Stdio::null())
            // npm's progress would land in `gruntas pi extension --json`'s output.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "npm is not on the PATH".to_string() } else { format!("npm could not start: {e}") })?;
        eprintln!("gruntas: fetching the Claude Agent SDK {version} from npm, once");
        let mut stderr = child.stderr.take();
        let said = std::thread::spawn(move || {
            let mut said = String::new();
            if let Some(e) = stderr.as_mut() {
                let _ = std::io::Read::read_to_string(e, &mut said);
            }
            said
        });
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < SDK_FETCH_DEADLINE => std::thread::sleep(std::time::Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("npm took longer than {} s", SDK_FETCH_DEADLINE.as_secs()));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("npm: {e}"));
                }
            }
        };
        if !status.success() {
            let said = said.join().unwrap_or_default();
            let said: Vec<&str> = said.lines().map(str::trim).filter(|l| !l.is_empty()).take(2).collect();
            return Err(format!("`npm ci` failed ({})", if said.is_empty() { status.to_string() } else { said.join("; ") }));
        }
        let fetched = stage.join("node_modules");
        if !sdk_is(&fetched.join(SDK), version) {
            return Err(format!("`npm ci` did not install {SDK} {version}"));
        }
        let target = dir.join("node_modules");
        // Another version, or a broken install: aside into the stage, which goes with it.
        let old = stage.join("old");
        let moved = std::fs::rename(&target, &old).is_ok();
        match std::fs::rename(&fetched, &target) {
            Ok(()) => Ok(()),
            // Another gruntas writing the extension at the same time got there first.
            Err(_) if sdk_is(&target.join(SDK), version) => Ok(()),
            Err(e) => {
                if moved && !target.exists() {
                    let _ = std::fs::rename(&old, &target);
                }
                Err(format!("{} could not be moved into place: {e}", fetched.display()))
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// `gruntas pi extension`: writes the carried files and prints where they are.
pub(super) fn extension(ctx: &mut Ctx) -> Result<(), Error> {
    let dir = dir(ctx.io.env).ok_or_else(|| fail("no_home", "there is no home directory to keep the pi extension in — set HOME"))?;
    let c = install(&dir).map_err(|e| fail("write_failed", format!("the pi extension could not be written to {}: {e}", dir.display())))?;
    if ctx.format != Format::Human {
        let summary = format!("pi extension {} at {}", super::VERSION, c.extension.display());
        let data = json!({ "extension": c.extension, "skill": c.skill, "version": super::VERSION, "written": c.written });
        return super::sessions::emit_data(ctx, data, summary);
    }
    let _ = writeln!(ctx.io.stdout, "extension  {}\nskill      {}", c.extension.display(), c.skill.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_carried_files_are_written_once_and_stamped_with_this_version() {
        let dir = std::env::temp_dir().join(format!("gruntas-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = write(&dir).unwrap();
        assert_eq!(first.written, vec![dir.join("gruntas.js"), dir.join("skills/gruntas/SKILL.md")]);
        let js = std::fs::read_to_string(&first.extension).unwrap();
        assert!(!js.contains(VERSION_TOKEN) && js.contains(&format!("\"{}\"", super::super::VERSION)));
        assert_eq!(std::fs::read_to_string(first.skill.join("SKILL.md")).unwrap(), SKILL);
        let again = write(&dir).unwrap();
        assert_eq!((again.extension, again.skill, again.written), (first.extension, first.skill.clone(), vec![]));
        std::fs::write(first.skill.join("SKILL.md"), "old").unwrap();
        assert_eq!(write(&dir).unwrap().written, vec![dir.join("skills/gruntas/SKILL.md")]);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temporary file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_embedded_bundle_carries_the_version_token_once_stamped_away() {
        assert!(EXTENSION.starts_with("// @gruntas/pi carried extension: "));
        assert!(EXTENSION.contains(VERSION_TOKEN));
    }

    #[test]
    fn the_sdk_pin_is_exact_and_integrity_checked() {
        let package: serde_json::Value = serde_json::from_str(SDK_PACKAGE).unwrap();
        let version = sdk_version().expect("the lockfile pins the SDK");
        assert_eq!(package["dependencies"][SDK], version, "package.json names the exact version, no range");
        let lock: serde_json::Value = serde_json::from_str(SDK_LOCK).unwrap();
        assert!(lock["packages"][format!("node_modules/{SDK}")]["integrity"].as_str().is_some_and(|i| i.starts_with("sha512-")));
    }

    /// A stand-in npm, run by `sh`: records its arguments and what it was
    /// given, then installs a package named `name` at `version` (or fails).
    fn fake_npm(root: &Path, name: &str, version: &str, exit: i32) -> PathBuf {
        let script = root.join("npm.sh");
        let log = root.join("npm.log");
        std::fs::write(
            &script,
            format!(
                "echo \"$@\" >> '{log}'\ncp package-lock.json '{root}/given-lock.json'\n[ {exit} = 0 ] || {{ echo 'npm error code ENOTFOUND' >&2; exit {exit}; }}\nmkdir -p node_modules/{SDK}\nprintf '{{\"name\":\"{name}\",\"version\":\"{version}\"}}' > node_modules/{SDK}/package.json\n",
                log = log.display(),
                root = root.display(),
            ),
        )
        .unwrap();
        script
    }

    #[test]
    fn the_sdk_is_fetched_once_with_the_pin_and_only_inside_the_pi_directory() {
        let root = std::env::temp_dir().join(format!("gruntas-pi-sdk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("pi");
        std::fs::create_dir_all(&dir).unwrap();
        let version = sdk_version().unwrap();
        let script = fake_npm(&root, SDK, &version, 0);
        let npm = [OsStr::new("sh"), script.as_os_str()];

        let sdk = dir.join("node_modules").join(SDK);
        assert_eq!(ensure_sdk(&dir, &npm), Some(sdk.clone()));
        assert!(sdk_is(&sdk, &version));
        assert_eq!(std::fs::read_to_string(root.join("given-lock.json")).unwrap(), SDK_LOCK, "npm ci ran on the embedded lockfile");
        let args = std::fs::read_to_string(root.join("npm.log")).unwrap();
        let stage = dir.join(format!(".sdk.{}", std::process::id()));
        for want in ["ci ", "--ignore-scripts", "--omit=optional", "--global=false", &format!("--prefix {}", stage.display()), &format!("--cache {}", stage.join(".npm-cache").display())] {
            assert!(args.contains(want), "npm ran with {want}: {args}");
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["node_modules"], "the staging directory is gone");

        // Present: npm isn't run again.
        assert_eq!(ensure_sdk(&dir, &npm), None);
        assert_eq!(std::fs::read_to_string(root.join("npm.log")).unwrap().lines().count(), 1);

        // Another version beside it is replaced; a failed fetch leaves what was there and no stage.
        std::fs::write(sdk.join("package.json"), format!(r#"{{"name":"{SDK}","version":"0.0.1"}}"#)).unwrap();
        assert_eq!(ensure_sdk(&dir, &npm), Some(sdk.clone()));
        assert!(sdk_is(&sdk, &version));
        std::fs::write(sdk.join("package.json"), format!(r#"{{"name":"{SDK}","version":"0.0.1"}}"#)).unwrap();
        let failing = fake_npm(&root, SDK, &version, 1);
        assert_eq!(ensure_sdk(&dir, &[OsStr::new("sh"), failing.as_os_str()]), None);
        assert!(sdk_is(&sdk, "0.0.1"));
        let wrong = fake_npm(&root, "something-else", &version, 0);
        assert_eq!(ensure_sdk(&dir, &[OsStr::new("sh"), wrong.as_os_str()]), None);
        assert!(sdk_is(&sdk, "0.0.1"));
        assert_eq!(ensure_sdk(&dir, &[OsStr::new("/nonexistent/npm")]), None, "no npm: no crash");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["node_modules"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
