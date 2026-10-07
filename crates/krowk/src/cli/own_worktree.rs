//! `--worktree` (worktrees WT6): a session started in a git worktree of its
//! own, made from the current directory's repository at its working state,
//! so sessions in separate terminals stop sharing one checkout. The
//! worktree is made, readied and finished by `krowk_harness::worktree`, as a
//! subagent's is; this is the command line around it, for `krowk -p` and
//! the TUI alike:
//!
//! - **Made before the session**, whose id is chosen first: the worktree is
//!   locked in that session's name, and the host gives the id to its first
//!   new session (`HostConfig::session`), with the prepare steps' notes at
//!   the top of its first prompt and `KROWK_PORT_BASE` in its commands'
//!   environment.
//! - **Held by this process** for the session's life (`InUse`), so the
//!   session runs in this process, never in the host daemon, which would
//!   outlive both the hold and the finish below.
//! - **Finished on the way out**, as a subagent's: unchanged, it is removed
//!   with its branch; otherwise it is kept and named, as
//!   `Worktree kept: <path> (branch krowk/<hex>)` on stderr, or as the
//!   `worktree` object of the result `-p` prints as JSON.
//! - **Resumed where it ran**: a session whose directory is a krowk
//!   worktree is held there again, and finished the same way. One whose
//!   directory is gone is refused, naming `krowk worktrees`.
//! - **Trusted as its repository** (`TrustAs`): the trust list names the
//!   repository a person trusted, never a worktree of it in krowk's data
//!   directory.
//!
//! WT12 will make a `--worktree` session take one of the machine's agent
//! slots as well: `open`, and `resumed`, are where it is taken.

use super::{prompt, Ctx};
use krowk_api::{fail, Error};
use krowk_harness::instances::Registry;
use krowk_harness::permissions;
use krowk_harness::trust;
use krowk_harness::worktree::{self, manage, Finished, InUse, Worktree};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// What `--worktree` outside a git repository is refused with.
pub(super) const NEEDS_A_REPOSITORY: &str = "--worktree makes a git worktree of the repository you are in, and this directory is in none — run krowk from inside a repository, or drop --worktree";

/// Where krowk's worktrees are.
fn root(ctx: &Ctx) -> Result<PathBuf, Error> {
    krowk_api::home::worktrees_root(ctx.io.env).ok_or_else(|| fail("no_home", "krowk found no home directory to make worktrees under — set HOME or XDG_DATA_HOME"))
}

/// A new worktree of the repository `cwd` is in, for the new session
/// `session`, readied by the prepare steps: it, held, and the notes for the
/// session's first prompt. `permissions` and `registry` are the session's:
/// whether the repository is trusted, its own `worktrees`, the person's.
pub(super) fn open(ctx: &Ctx, cwd: &Path, permissions: &permissions::Config, registry: &Registry, session: &str) -> Result<(InUse, Vec<String>), Error> {
    let root = root(ctx)?;
    let policy = permissions::Policy::load(permissions, cwd).map_err(prompt::bad_settings)?;
    let runtime = krowk_harness::slots::runtime_dir(ctx.io.env);
    let readying = worktree::Readying { policy: &policy, config: &registry.worktrees, builds: Some(&registry.builds), cancel: None, runtime };
    worktree::open(cwd, &root, session, &readying).map_err(|e| match e {
        worktree::Error::NotARepository => fail("not_a_repository", NEEDS_A_REPOSITORY),
        e => fail("worktree_failed", format!("the session's worktree could not be made: {e}")),
    })
}

/// The krowk worktree the session `session` ran in, `dir`, held again for
/// it to go on there: none when `dir` is not one of krowk's worktrees.
/// Refused when it is gone, or another live session holds it.
pub(super) fn resumed(ctx: &mut Ctx, session: &str, dir: &Path) -> Result<Option<InUse>, Error> {
    let Ok(root) = root(ctx) else { return Ok(None) };
    let real = root.canonicalize().unwrap_or_else(|_| root.clone());
    if dir == root || dir == real || !(dir.starts_with(&root) || dir.starts_with(&real)) {
        return Ok(None);
    }
    if !dir.is_dir() {
        return Err(fail(
            "worktree_missing",
            format!("session {session} ran in the krowk worktree {}, which is gone — `krowk worktrees` lists the ones that remain; or start a fresh session, without --resume", dir.display()),
        ));
    }
    let Some(wt) = manage::find(&root, &dir.display().to_string()).ok().and_then(|l| l.worktree()) else { return Ok(None) };
    let held = manage::hold_again(&wt).map_err(|e| fail("worktree_failed", format!("the worktree {} could not be held for the session: {e}", wt.path.display())))?;
    let Some(held) = held else {
        return Err(fail("worktree_live", format!("another krowk session works in {} now — resume this one once it has ended", wt.path.display())));
    };
    let (in_use, missing) = InUse::new(wt, held, krowk_harness::slots::runtime_dir(ctx.io.env));
    if let Some(why) = missing {
        ctx.warn(&why);
    }
    Ok(Some(in_use))
}

/// What the session's worktree came to, said on stderr: nothing when it
/// was removed, the line naming it when it was kept, and why when it could
/// not be judged (it is then left as it is).
pub(super) fn report(ctx: &mut Ctx, wt: &Worktree, finished: &Result<Finished, worktree::Error>) {
    match finished {
        Ok(Finished::Removed) => {}
        Ok(Finished::Kept { .. }) => {
            let _ = writeln!(ctx.io.stderr, "Worktree kept: {} (branch {})", wt.path.display(), wt.branch());
        }
        Err(e) => ctx.warn(&format!("the worktree {} (branch {}) was left as it is: {e} — `krowk worktrees` lists it", wt.path.display(), wt.branch())),
    }
}

/// The repository a krowk worktree this process works in is trusted as:
/// its main checkout. The trust list names the repository a person
/// trusted, and a worktree of it, under krowk's data directory, is that
/// repository's content. Set once the worktree is known, and read by every
/// trust question the session's settings, gate and routing ask, each of
/// which is handed the worktree's top (`trust::root`).
#[derive(Clone, Default)]
pub(super) struct TrustAs(Arc<OnceLock<(PathBuf, PathBuf)>>);

impl TrustAs {
    pub(super) fn set(&self, wt: &Worktree) {
        let _ = self.0.set((trust::root(&wt.path), trust::root(&wt.main)));
    }

    /// `root`, or the repository it stands for.
    pub(super) fn root(&self, root: &Path) -> PathBuf {
        match self.0.get() {
            Some((wt, repo)) if root == wt => repo.clone(),
            _ => root.to_path_buf(),
        }
    }
}
