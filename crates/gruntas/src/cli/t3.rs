//! `gruntas t3` (T3 Code D4, R-T3-5): one launcher script per connected
//! Claude Code and Codex account, which T3 Code runs as an instance's
//! binary, and the instance to add for each: its name, binary path and
//! home path. T3 Code spawns the binary path directly, as one executable,
//! so the script is what turns it into `gruntas run … --passthrough`.

use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Error};
use gruntas_harness::instances::Registry;
use gruntas_harness::launch::{self, Account, Harness};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The scripts' directory under the gruntas home: gruntas's own, so a script
/// there that names no account any more is gruntas's to remove.
const DIR: &str = "t3";

/// A script's second line: what tells a script `gruntas t3` wrote, which it
/// may rewrite or remove, from anything else in its directory.
const MARK: &str = "# Written by `gruntas t3` for T3 Code; rerun it to update. Edits are overwritten.";

/// One account's T3 Code instance.
struct Launcher {
    /// The account's instance, `claude:work`.
    account: String,
    harness: Harness,
    /// T3 Code's instance name, `Claude · work`.
    name: String,
    script: PathBuf,
    /// The account's config home (CLAUDE_CONFIG_DIR, CODEX_HOME), which T3
    /// Code reads itself for skills, usage and resume.
    home: PathBuf,
    /// Whether this run wrote its script.
    written: bool,
}

pub(super) fn t3(ctx: &mut Ctx) -> Result<(), Error> {
    let registry = Registry::resolve(&super::prompt::load_instances()?, ctx.io.env);
    let dir = gruntas_api::home::dir(ctx.io.env)?.join(DIR);
    // Written into and removed from as gruntas's own: never through a link.
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
        return Err(fail("write_failed", format!("{} is not a directory, so gruntas cannot keep T3 Code's launchers there — move it aside", dir.display())));
    }
    let gruntas = gruntas_path(ctx.io.env)?;
    let (reached, unreached) = accounts(&registry);
    for name in &unreached {
        ctx.warn(&format!("no launcher for {name:?}, which `gruntas run --account` cannot name — rename it with `gruntas providers rename`"));
    }
    let mut launchers = Vec::new();
    // Its launcher kept as it is: an account gruntas cannot give one now.
    let mut kept = Vec::new();
    for (account, short, harness) in reached {
        let Some(home) = registry.instances[&account].backend.as_ref().and_then(|b| b.home.clone()) else {
            ctx.warn(&format!("no config home for {account} to give T3 Code — set HOME — so its launcher is left as it was"));
            kept.push(dir.join(account.replace(':', "-")));
            continue;
        };
        let label = match harness {
            Harness::Claude => "Claude",
            Harness::Codex => "Codex",
        };
        let name = format!("{label} · {}", if short == harness.name() { "default" } else { short.as_str() });
        let script = dir.join(account.replace(':', "-"));
        let content = script_for(&gruntas, harness, &short);
        let written = write(&dir, &script, &content).map_err(|e| fail("write_failed", format!("the T3 Code launcher {} could not be written: {e}", script.display())))?;
        launchers.push(Launcher { account, harness, name, script, home, written });
    }
    let keep: Vec<&Path> = launchers.iter().map(|l| l.script.as_path()).chain(kept.iter().map(PathBuf::as_path)).collect();
    let removed = remove_stale(&dir, &keep).map_err(|e| fail("write_failed", format!("the T3 Code launchers of accounts that are gone could not be removed from {}: {e}", dir.display())))?;
    // Nothing connected, as against accounts with no launcher this time.
    let none = kept.is_empty() && unreached.is_empty();
    if ctx.format != Format::Human {
        let instances: Vec<Value> = launchers
            .iter()
            .map(|l| json!({ "account": l.account, "harness": l.harness.name(), "name": l.name, "binaryPath": l.script, "homePath": l.home, "written": l.written }))
            .collect();
        let summary = match launchers.len() {
            0 if none => "No Claude Code or Codex account is connected — connect one with `gruntas connect`".to_string(),
            0 => "No T3 Code launcher could be written: the warnings say why".to_string(),
            n => format!("{n} T3 Code instance{} to add", if n == 1 { "" } else { "s" }),
        };
        return super::sessions::emit_data(ctx, json!({ "directory": dir, "instances": instances, "removed": removed }), summary);
    }
    let out = &mut *ctx.io.stdout;
    if launchers.is_empty() && !none {
        let _ = writeln!(out, "No T3 Code launcher could be written: the warnings above say why.");
    } else if launchers.is_empty() {
        let _ = writeln!(out, "No Claude Code or Codex account is connected, so T3 Code has nothing to run through gruntas. Connect one with `gruntas connect`, then run `gruntas t3` again.");
    } else {
        let _ = writeln!(out, "In T3 Code, add one instance per account:");
        for l in &launchers {
            let _ = writeln!(out, "\n{}\n  Binary path  {}\n  Home path    {}", l.name, l.script.display(), l.home.display());
        }
    }
    for r in &removed {
        let _ = writeln!(out, "\nRemoved {}, whose account is gone: remove its instance from T3 Code too.", r.display());
    }
    Ok(())
}

/// The connected (configured) Claude Code and Codex accounts that `gruntas
/// run --account` reaches: each instance, the name `--account` takes, and
/// its harness. One whose name `--account` would read as another account
/// (`claude:auto`), that is not `<harness>:<name>`, or that no file could be
/// named after is in the second list instead.
fn accounts(registry: &Registry) -> (Vec<(String, String, Harness)>, Vec<String>) {
    let (mut out, mut unreached) = (Vec::new(), Vec::new());
    for name in &registry.configured {
        for harness in [Harness::Claude, Harness::Codex] {
            if launch::account(registry, harness, name).is_err() {
                continue;
            }
            let short = match name.split_once(':') {
                Some((pre, rest)) if pre == harness.name() => rest.to_string(),
                None if name == harness.name() => name.clone(),
                _ => {
                    unreached.push(name.clone());
                    continue;
                }
            };
            let reaches = Account::parse(harness, &short).ok().and_then(|a| a.instance(harness));
            if reaches.as_deref() == Some(name.as_str()) && !short.chars().any(|c| c == '/' || c.is_control()) {
                out.push((name.clone(), short, harness));
            } else {
                unreached.push(name.clone());
            }
        }
    }
    (out, unreached)
}

/// The gruntas a script runs, by an absolute path, since T3 Code may spawn
/// it with a PATH that has no gruntas on it: the one on this shell's PATH
/// when that is this binary (a package manager's link, which an upgrade
/// keeps pointing at the new version), else this binary's own path.
fn gruntas_path(env: &dyn Fn(&str) -> String) -> Result<PathBuf, Error> {
    let own = std::env::current_exe().map_err(|e| fail("no_binary", format!("this gruntas's own path cannot be read, which the launchers run: {e}")))?;
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    let on_path = gruntas_harness::instances::find_binary("gruntas", &env("PATH")).filter(|p| p.is_absolute() && canonical(p).is_some() && canonical(p) == canonical(&own));
    let path = on_path.unwrap_or(own);
    if path.to_str().is_none() {
        return Err(fail("no_binary", format!("this gruntas's path, {}, is not UTF-8, so a launcher cannot name it", path.display())));
    }
    Ok(path)
}

/// The script: `exec gruntas run <harness> --account <name> --passthrough
/// -- "$@"`, the path and the name quoted for sh.
fn script_for(gruntas: &Path, harness: Harness, account: &str) -> String {
    let quote = gruntas_harness::worktree::manage::quote;
    format!("#!/bin/sh\n{MARK}\nexec {} run {} --account {} --passthrough -- \"$@\"\n", quote(gruntas), harness.name(), quote(Path::new(account)))
}

/// The mode a script has: the person's alone to read and run.
const MODE: u32 = 0o700;

/// `content` at `script`, executable, unless it is there already as a
/// regular file with that content and mode: written to a file beside it and
/// renamed over it, so T3 Code never runs half a script, and a link there
/// is replaced rather than followed. Whether it was written.
fn write(dir: &Path, script: &Path, content: &str) -> std::io::Result<bool> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let same = std::fs::symlink_metadata(script).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o7777 == MODE) && std::fs::read(script).is_ok_and(|had| had == content.as_bytes());
    if same {
        return Ok(false);
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let name = script.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(MODE).open(&tmp)?;
        std::io::Write::write_all(&mut f, content.as_bytes())?;
        // The umask may have narrowed it; MODE has nothing it would widen.
        f.set_permissions(std::fs::Permissions::from_mode(MODE))?;
        f.sync_all()?;
        std::fs::rename(&tmp, script)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| true)
}

/// Removes each script in `dir` that `gruntas t3` wrote (`MARK`) and that is
/// not in `keep`: an account that is gone. What it removed.
fn remove_stale(dir: &Path, keep: &[&Path]) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // A dot file is another run's script on its way in (`write`).
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let ours = !hidden && entry.file_type().is_ok_and(|t| t.is_file()) && std::fs::read_to_string(&path).is_ok_and(|s| s.lines().nth(1) == Some(MARK));
        if ours && !keep.contains(&path.as_path()) {
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    removed.sort();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_t3_5_the_script_quotes_the_path_and_the_account_for_sh() {
        let s = script_for(Path::new("/opt/my apps/gruntas"), Harness::Claude, "it's");
        assert_eq!(s.lines().last(), Some(r#"exec '/opt/my apps/gruntas' run claude --account 'it'\''s' --passthrough -- "$@""#));
        let s = script_for(Path::new("/usr/bin/gruntas"), Harness::Codex, "team");
        assert_eq!(s, format!("#!/bin/sh\n{MARK}\nexec /usr/bin/gruntas run codex --account team --passthrough -- \"$@\"\n"));
    }
}
