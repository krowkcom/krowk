//! `krowk worktrees`: the git worktrees krowk made for agents, across
//! repositories (WT9). Every agent that changed something leaves one, and
//! deleting one by hand strands git's record of it and can lose work. The
//! work is `krowk_harness::worktree::manage`; this is the command line
//! around it.
//!
//! - `list` (bare `krowk worktrees` too): each one's directory, repository,
//!   branch, base, commits ahead of the base, uncommitted changes, session,
//!   age, and whether a live session holds it.
//! - `remove <hex|path> [--force]`: refused (exit 4) while a live session
//!   holds it (`worktree_live`), and while it has uncommitted changes or
//!   commits ahead of its base unless forced (`worktree_has_changes`).
//!   Forced, its uncommitted changes are saved as
//!   `refs/krowk/snapshots/<hex>` first, and a HEAD off its branch as
//!   `refs/krowk/snapshots/<hex>-head`. Its branch is always kept, and the
//!   answer says how to bring it back. Ignored files (build output,
//!   `.worktreeinclude` copies) are not changes, and are deleted with it.
//! - `apply <hex|path> [--to <dir>]` (WT13): its commits and uncommitted
//!   changes applied to the working tree of `--to`, the repository's main
//!   checkout by default, as uncommitted changes, never its index or HEAD;
//!   then it and its branch are removed, its final state kept as
//!   `refs/krowk/snapshots/<hex>`. Refused (exit 4) while a live session
//!   holds it (`worktree_live`), when a file does not apply cleanly
//!   (`worktree_conflicts`, nothing changed, the files named), and when it
//!   changed submodules (`worktree_submodules`); `--to` a checkout of
//!   another repository is refused (exit 1, `worktree_other_repository`).
//!   Changes inside `.git`, `.claude`, `.codex` and `.krowk` directories,
//!   which a subagent's apply-back leaves to a person, are applied: this is
//!   the person's own command. How a person brings a
//!   `--worktree` session's work home.
//! - `prune`: clears git's record of worktrees whose directory is gone,
//!   deletes directories whose record is gone, and snapshots over 30 days
//!   old. It also runs by itself once a day when a session starts.
//!
//! `--json` rows: `{hex, path, repo, branch, base, ahead, dirty, session,
//! created_ms, age_seconds, live, missing}`, every key on every row, `null`
//! where it is not known.

use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::worktree::apply::{named, Applied};
use krowk_harness::worktree::manage::{self, Listed, Refusal, SNAPSHOTS};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::SystemTime;

/// Where krowk's worktrees are.
fn root(ctx: &Ctx) -> Result<PathBuf, Error> {
    krowk_api::home::worktrees_root(ctx.io.env).ok_or_else(|| fail("no_home", "krowk found no home directory to look for its worktrees under — set HOME or XDG_DATA_HOME"))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// One worktree as `--json` has it.
fn row(l: &Listed, now: i64) -> Value {
    json!({
        "hex": l.hex,
        "path": l.path.display().to_string(),
        "repo": l.main.display().to_string(),
        "branch": l.branch,
        "base": l.base,
        "ahead": l.ahead,
        "dirty": l.dirty,
        "session": l.session,
        "created_ms": l.created_ms,
        "age_seconds": l.created_ms.map(|c| (now - c).max(0) / 1000),
        "live": l.live,
        "missing": l.missing,
    })
}

/// "3m", "5h", "2d": how long ago, short.
fn age(created: Option<i64>, now: i64) -> String {
    let Some(created) = created else { return "?".into() };
    let s = (now - created).max(0) / 1000;
    match s {
        s if s < 60 * 60 => format!("{}m", s / 60),
        s if s < 48 * 60 * 60 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// `yes`, `no`, or `?` when not known.
fn yes(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    }
}

pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    let root = root(ctx)?;
    let all = manage::list(&root);
    let now = now_ms();
    let live = all.iter().filter(|l| l.live).count();
    let summary = format!("{} worktree{} ({live} live)", all.len(), if all.len() == 1 { "" } else { "s" });
    if ctx.format != Format::Human {
        let rows: Vec<Value> = all.iter().map(|l| row(l, now)).collect();
        return super::sessions::emit_data(ctx, json!({ "worktrees": rows }), summary);
    }
    let out = &mut *ctx.io.stdout;
    if all.is_empty() {
        let _ = writeln!(out, "No krowk worktrees.");
        return Ok(());
    }
    let cells: Vec<[String; 8]> = all
        .iter()
        .map(|l| {
            let ahead = l.ahead.map_or("?".into(), |n| n.to_string());
            let dirty = if l.missing { "gone".to_string() } else { yes(l.dirty).to_string() };
            let session = l.session.as_deref().map_or("-".to_string(), |s| s.chars().take(8).collect());
            [l.hex.clone(), l.branch.clone().unwrap_or_else(|| "(detached)".into()), ahead, dirty, yes(Some(l.live)).into(), session, age(l.created_ms, now), l.path.display().to_string()]
        })
        .collect();
    let head = ["WORKTREE", "BRANCH", "AHEAD", "DIRTY", "LIVE", "SESSION", "AGE", "PATH"].map(String::from);
    let mut width = [0usize; 7];
    for r in std::iter::once(&head).chain(&cells) {
        for (w, c) in width.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    for r in std::iter::once(&head).chain(&cells) {
        let line: Vec<String> = r[..7].iter().zip(width).map(|(c, w)| format!("{c:<w$}")).collect();
        let _ = writeln!(out, "{}  {}", line.join("  "), r[7]);
    }
    let _ = writeln!(out, "{summary}");
    Ok(())
}

pub(super) fn remove(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let key = args.first().map(|a| a.trim()).filter(|a| !a.is_empty()).ok_or_else(|| fail("missing_argument", "name the worktree to remove: `krowk worktrees remove <hex|path>` — `krowk worktrees` lists them"))?;
    let root = root(ctx)?;
    let human = ctx.format == Format::Human;
    let removed = manage::remove(&root, key, ctx.f.force).map_err(|r| refused(r, now_ms(), !human))?;
    let l = &removed.listed;
    let restore = removed.restore();
    let mut summary = format!("Removed {}. Branch {} is kept", l.path.display(), l.own_branch());
    if let Some(h) = &removed.head {
        summary.push_str(&format!("; its HEAD, which that branch does not hold, is saved as {h}"));
    }
    if let Some(r) = &removed.snapshot {
        summary.push_str(&format!("; its uncommitted changes are saved as {r}"));
    }
    if !human {
        let data = json!({ "removed": row(l, now_ms()), "branch": l.own_branch(), "snapshot": removed.snapshot, "head": removed.head, "restore": restore });
        return super::sessions::emit_data(ctx, data, summary);
    }
    let _ = writeln!(ctx.io.stdout, "{summary}.\nTo bring it back:\n  {}", restore.join("\n  "));
    Ok(())
}

pub(super) fn apply(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let key = args.first().map(|a| a.trim()).filter(|a| !a.is_empty()).ok_or_else(|| fail("missing_argument", "name the worktree to apply: `krowk worktrees apply <hex|path>` — `krowk worktrees` lists them"))?;
    let root = root(ctx)?;
    let human = ctx.format == Format::Human;
    let to = match ctx.f.to.trim() {
        "" => None,
        dir => Some(std::path::absolute(dir).map_err(|e| fail("bad_flag", format!("--to {dir}: {e}")))?),
    };
    let (l, applied) = manage::apply(&root, key, to.as_deref()).map_err(|r| refused(r, now_ms(), !human))?;
    let target = to.unwrap_or_else(|| l.main.clone());
    let snapshot = format!("{SNAPSHOTS}{}", l.hex);
    let kept = |mut e: Error, files: &[String]| {
        if !human {
            e.body.insert("details".into(), json!({ "worktree": row(&l, now_ms()), "files": files }));
        }
        e
    };
    let (files, left) = match applied {
        Applied::Applied(files) => (files, None),
        Applied::AppliedLeft(files, why) => (files, Some(why)),
        // A person's command applies fenced files; never one of these.
        Applied::Protected(files) => {
            let fix = format!("{} changes protected files ({}) — nothing was changed; the worktree and its branch {} are kept", l.path.display(), named(&files), l.own_branch());
            return Err(kept(fail("worktree_protected", fix), &files));
        }
        Applied::Conflicts(files) => {
            let fix = format!(
                "{}'s changes do not apply cleanly to {} (conflicts in {}) — nothing was changed. The worktree and its branch {} are kept: merge that branch, or copy what you need from the worktree",
                l.path.display(),
                target.display(),
                named(&files),
                l.own_branch()
            );
            return Err(kept(fail("worktree_conflicts", fix), &files));
        }
        Applied::Submodules(paths) => {
            let fix = format!("{} changed submodules ({}), which krowk does not apply — nothing was changed. The worktree and its branch {} are kept: bring those changes over by hand", l.path.display(), named(&paths), l.own_branch());
            return Err(kept(fail("worktree_submodules", fix), &paths));
        }
    };
    let n = files.len();
    let summary = match (&left, n) {
        (Some(why), _) => format!(
            "Applied {n} changed file{} from {} to {}, but could not remove it afterwards ({why}) — do not apply it again; `krowk worktrees remove {} --force` removes it",
            if n == 1 { "" } else { "s" },
            l.path.display(),
            target.display(),
            l.hex
        ),
        (None, 0) => format!("{} had no changes left to apply; it and its branch {} are removed", l.path.display(), l.own_branch()),
        (None, n) => format!("Applied {n} changed file{} from {} to {}; it and its branch {} are removed, its final state kept as {snapshot}", if n == 1 { "" } else { "s" }, l.path.display(), target.display(), l.own_branch()),
    };
    if !human {
        let data = json!({ "applied": row(&l, now_ms()), "to": target.display().to_string(), "files": files, "snapshot": snapshot, "removed": left.is_none() });
        return super::sessions::emit_data(ctx, data, summary);
    }
    let out = &mut *ctx.io.stdout;
    for f in &files {
        let _ = writeln!(out, "applied  {f}");
    }
    let _ = writeln!(out, "{summary}.");
    Ok(())
}

/// A refusal as krowk's error: its code and fix, and, when `details`, the
/// worktree it is about as the error's `details`, for a script to read; a
/// person has the fix.
fn refused(r: Refusal, now: i64, details: bool) -> Error {
    let with = |mut e: Error, l: &Listed| {
        if details {
            e.body.insert("details".into(), row(l, now));
        }
        e
    };
    match r {
        Refusal::NotFound(key) => fail("no_such_worktree", format!("`{key}` names no krowk worktree — `krowk worktrees` lists them")),
        Refusal::Ambiguous(paths) => {
            let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            fail("ambiguous_worktree", format!("more than one krowk worktree is called that: {} — name it by its path", paths.join(", ")))
        }
        Refusal::Live(l) => {
            let session = l.session.as_deref().unwrap_or("a session");
            with(fail("worktree_live", format!("{} is in use by {session}, which is still running — try again once that session ends", l.path.display())), &l)
        }
        Refusal::Changed(l) => {
            let mut what = Vec::new();
            if l.dirty != Some(false) {
                what.push("uncommitted changes".to_string());
            }
            match l.ahead {
                Some(0) => {}
                Some(n) => what.push(format!("{n} commit{} ahead of its base", if n == 1 { "" } else { "s" })),
                None => what.push("a base krowk has no record of".to_string()),
            }
            let fix = format!(
                "{} has {} — nothing was removed. `--force` removes it anyway, saving uncommitted changes as refs/krowk/snapshots/{} first; its branch {} is kept either way",
                l.path.display(),
                what.join(" and "),
                l.hex,
                l.own_branch()
            );
            with(fail("worktree_has_changes", fix), &l)
        }
        Refusal::Failed(krowk_harness::worktree::Error::OtherRepository(why)) => fail("worktree_other_repository", format!("{why} — name a checkout of that repository with --to")),
        Refusal::Failed(e) => fail("worktree_failed", e.to_string()),
    }
}

pub(super) fn prune(ctx: &mut Ctx) -> Result<(), Error> {
    let root = root(ctx)?;
    let done = manage::prune(&root, SystemTime::now());
    let paths = |p: &[PathBuf]| p.iter().map(|p| p.display().to_string()).collect::<Vec<_>>();
    let summary = format!(
        "{} stale registration{}, {} {} and {} snapshot{} pruned",
        done.registrations.len(),
        if done.registrations.len() == 1 { "" } else { "s" },
        done.dirs.len(),
        if done.dirs.len() == 1 { "directory" } else { "directories" },
        done.snapshots.len(),
        if done.snapshots.len() == 1 { "" } else { "s" },
    );
    for f in &done.failures {
        ctx.warn(&format!("could not prune {f}"));
    }
    if ctx.format != Format::Human {
        let data = json!({ "registrations": paths(&done.registrations), "dirs": paths(&done.dirs), "snapshots": done.snapshots, "failures": done.failures });
        return super::sessions::emit_data(ctx, data, summary);
    }
    let out = &mut *ctx.io.stdout;
    for p in &done.registrations {
        let _ = writeln!(out, "cleared  {}", p.display());
    }
    for p in &done.dirs {
        let _ = writeln!(out, "deleted  {}", p.display());
    }
    for s in &done.snapshots {
        let _ = writeln!(out, "expired  {s}");
    }
    let _ = writeln!(out, "{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt9_an_age_is_short() {
        let now = 10 * 86_400_000;
        assert_eq!(age(Some(now - 5 * 60_000), now), "5m");
        assert_eq!(age(Some(now - 3 * 3_600_000), now), "3h");
        assert_eq!(age(Some(now - 3 * 86_400_000), now), "3d");
        assert_eq!(age(None, now), "?");
    }
}
