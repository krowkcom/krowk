//! `krowk trust`: which repositories a backend or the agent may run what
//! they configure in, grant, revoke and list, with no terminal needed. The
//! list is the one `-p`, the host and `sync take` read.

use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::trust;
use krowk_harness::worktree::manage;
use serde_json::json;
use std::path::PathBuf;

fn store(ctx: &Ctx) -> Result<trust::Store, Error> {
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(PathBuf::from);
    Ok(trust::Store::new(Some(super::providers::krowk_dir()?.join(trust::FILE)), home))
}

/// The repository `arg` (default: the working directory) belongs to,
/// canonical. A krowk worktree is its main repository, as `-p` trusts it.
fn repository(ctx: &Ctx, arg: Option<&str>) -> Result<PathBuf, Error> {
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    let dir = arg.map_or(cwd.clone(), |a| cwd.join(a));
    if !dir.is_dir() {
        return Err(fail("not_a_directory", format!("{} is not a directory", dir.display())));
    }
    let root = trust::root(&dir);
    let worktrees = krowk_api::home::worktrees_root(ctx.io.env).map(|r| manage::list(&r)).unwrap_or_default();
    Ok(worktrees.iter().find(|l| !l.missing && l.path.canonicalize().is_ok_and(|p| trust::root(&p) == root)).map_or(root, |l| trust::root(&l.main)))
}

pub(super) fn run(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let (list, revoke) = (ctx.f.list, ctx.f.revoke);
    if list && revoke {
        return Err(fail("bad_flag", "`--list` and `--revoke` do different things: pass one"));
    }
    if args.len() > 1 {
        return Err(fail("unexpected_argument", format!("`krowk trust` takes one directory, and got `{}`", args.join(" "))));
    }
    if list && !args.is_empty() {
        return Err(fail("unexpected_argument", "`krowk trust --list` lists every directory and takes no path"));
    }
    let store = store(ctx)?;
    if list {
        let dirs = store.recorded().map_err(|e| fail("bad_config", e))?;
        if ctx.format != Format::Human {
            let shown: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
            let summary = format!("{} trusted director{}", dirs.len(), if dirs.len() == 1 { "y" } else { "ies" });
            return super::sessions::emit_data(ctx, json!({ "directories": shown }), summary);
        }
        let out = &mut *ctx.io.stdout;
        if dirs.is_empty() {
            let _ = writeln!(out, "No trusted repositories. `krowk trust` trusts the one you are in.");
        }
        for d in &dirs {
            let _ = writeln!(out, "{}", d.display());
        }
        return Ok(());
    }
    let root = repository(ctx, args.first().map(String::as_str))?;
    let shown = root.display().to_string();
    if revoke {
        let had = store.revoke(&root).map_err(|e| fail("trust_not_saved", e))?;
        if ctx.format != Format::Human {
            return super::sessions::emit_data(ctx, json!({ "directory": shown, "trusted": false }), format!("{shown} is not trusted"));
        }
        let _ = if had { writeln!(ctx.io.stdout, "No longer trusting {shown}") } else { writeln!(ctx.io.stdout, "{shown} was not trusted") };
        return Ok(());
    }
    store.trust(&root).map_err(|e| fail("cannot_trust", e))?;
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({ "directory": shown, "trusted": true }), format!("{shown} is trusted"));
    }
    let runs = trust::what_runs(&root);
    let _ = writeln!(ctx.io.stdout, "Trusting {shown}");
    if !runs.is_empty() {
        let _ = writeln!(ctx.io.stdout, "It has {}, which a backend or krowk's agent may now run there.", runs.join(", "));
    }
    Ok(())
}
