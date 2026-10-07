//! Applying a worktree's changes back (WT13): a subagent's worktree leaves
//! its work on a `krowk/<hex>` branch its parent cannot use — the parent's
//! sandbox keeps `.git` read-only, so it cannot merge — and a `--worktree`
//! session's person has the same gap. krowk, outside any sandbox, brings
//! the changes into a working tree as uncommitted changes:
//!
//! - **Its final state recorded** (`snapshot`): every change `git add
//!   --all` finds, commits and uncommitted edits alike, new and deleted
//!   files too, written as one commit on top of the worktree's HEAD
//!   through a copy of its index (never its own), kept as
//!   `refs/krowk/snapshots/<hex>`, which `manage::prune` expires after
//!   `manage::SNAPSHOT_DAYS`.
//! - **Applied whole or not at all**: the diff from the worktree's base to
//!   that commit, binary files included and renames as a deletion and an
//!   addition, `git apply --check`ed and then `git apply`d at the target's
//!   top level. Plain `git apply` writes the working tree only: the
//!   target's index and HEAD are never touched. Under the repository's
//!   lock, so siblings finishing at once apply one after the other, each
//!   onto what the last left.
//! - **Applied**: the worktree and its branch are removed; the snapshot is
//!   the way back. **Not applied** (a file the target changed too): both
//!   are kept, and the files that conflict are named.
//! - **Submodules** (WT15): a change to what a submodule records, or edits
//!   inside one, cannot be applied as a file change; a worktree with any is
//!   not applied, and is kept, so nothing of it is lost on the way.

use super::manage::SNAPSHOTS;
use super::{copy_index, git, has_submodules, query, random_hex, read, remove, submodule_changes, Error, Scratch, Worktree};
use std::path::{Path, PathBuf};

/// How many files a note names before "and N more".
const MAX_NAMED: usize = 20;

/// What applying a worktree's changes came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// In the target's working tree, the files it changed named; the
    /// worktree and its branch are gone. Empty when its changes undid each
    /// other.
    Applied(Vec<String>),
    /// Not applied, as these files do not apply cleanly; the worktree and
    /// its branch are kept.
    Conflicts(Vec<String>),
    /// Not applied, as it changed these submodules (or added a repository
    /// inside it); the worktree and its branch are kept.
    Submodules(Vec<String>),
}

impl Applied {
    /// The line the agent whose worktree it was is told, at the end of the
    /// child's summary: none when nothing was applied because nothing had
    /// changed.
    pub fn note(&self, wt: &Worktree) -> Option<String> {
        let kept = || format!("Worktree: {}, branch {}", wt.path.display(), wt.branch());
        match self {
            Applied::Applied(files) if files.is_empty() => None,
            Applied::Applied(files) => Some(format!("Changes applied to your working tree: {}", named(files))),
            Applied::Conflicts(files) => Some(format!("Changes not applied (conflicts in {}). {}", named(files), kept())),
            Applied::Submodules(paths) => Some(format!("Changes not applied (they change submodules, which krowk does not apply: {}). {}", named(paths), kept())),
        }
    }
}

/// `files`, at most `MAX_NAMED` of them, then "and N more".
pub fn named(files: &[String]) -> String {
    let mut out = files.iter().take(MAX_NAMED).cloned().collect::<Vec<_>>().join(", ");
    if files.len() > MAX_NAMED {
        out.push_str(&format!(" and {} more", files.len() - MAX_NAMED));
    }
    out
}

/// Applies the changes of `wt`, which no agent works in any more, to the
/// working tree `to` is in (see the module's docs): removed with its
/// branch when they apply, kept when they do not. Under the caller's
/// `super::lock`. Blocking.
pub(super) fn apply(wt: &Worktree, to: &Path) -> Result<Applied, Error> {
    let top = top_level(to)?;
    if wt.path.canonicalize().is_ok_and(|p| p == top) {
        return Err(Error::Failed(format!("{} is the worktree itself: name the checkout its changes go to", to.display())));
    }
    // Edits inside a submodule are in no commit of the worktree's.
    if has_submodules(&wt.path) {
        let mut inside: Vec<String> = submodule_changes(&wt.path, "", 0)?.lines().filter_map(|l| l.split_once(": ")).map(|(sub, _)| sub.trim_end_matches('/').to_string()).collect();
        inside.dedup();
        if !inside.is_empty() {
            return Ok(Applied::Submodules(inside));
        }
    }
    let snapshot = snapshot(wt)?;
    let (files, links) = changed(wt, &snapshot)?;
    if !links.is_empty() {
        return Ok(Applied::Submodules(links));
    }
    if !files.is_empty() {
        let patch = Scratch(std::path::absolute(wt.repo_dir().join(format!(".patch-{}", random_hex()))).map_err(|e| Error::Failed(format!("the patch: {e}")))?);
        diff(wt, &snapshot, &patch.0)?;
        if !git_apply(&top, &patch.0, &["--check"])? {
            // Asked file by file, so the files are named whatever language
            // git speaks.
            let mut conflicts = Vec::new();
            for f in &files {
                if !git_apply(&top, &patch.0, &["--check", &format!("--include={}", glob_escape(f))])? {
                    conflicts.push(f.clone());
                }
            }
            return Ok(Applied::Conflicts(if conflicts.is_empty() { files } else { conflicts }));
        }
        if !git_apply(&top, &patch.0, &[])? {
            return Err(Error::Failed(format!("git apply in {} failed after its check passed; nothing was removed", top.display())));
        }
    }
    let _ = read(git(&wt.main)?.args(["worktree", "unlock"]).arg(&wt.path), "worktree unlock");
    remove(wt, true)?;
    Ok(Applied::Applied(files))
}

/// The top level of the working tree `dir` is in, canonical.
fn top_level(dir: &Path) -> Result<PathBuf, Error> {
    let top = read(query(dir)?.args(["rev-parse", "--show-toplevel"]), "rev-parse").map_err(|_| Error::Failed(format!("{} is in no git checkout to apply the changes to", dir.display())))?;
    if top.is_empty() {
        return Err(Error::Failed(format!("{} is in no git checkout to apply the changes to", dir.display())));
    }
    PathBuf::from(&top).canonicalize().map_err(|e| Error::Failed(format!("{top}: {e}")))
}

/// The final state of `wt` as a commit on top of its HEAD, kept as
/// `refs/krowk/snapshots/<hex>`: built in a scratch index in its
/// repository's directory, a copy of its own (which is not touched), as
/// `super::working_state` builds a parent's.
fn snapshot(wt: &Worktree) -> Result<String, Error> {
    let index = Scratch(std::path::absolute(wt.repo_dir().join(format!(".index-{}", random_hex()))).map_err(|e| Error::Failed(format!("the snapshot's index: {e}")))?);
    let staged = || git(&wt.path).map(|mut c| {
        c.env("GIT_INDEX_FILE", &index.0).args(["-c", "core.splitIndex=false"]);
        c
    });
    let own = PathBuf::from(read(query(&wt.path)?.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]), "rev-parse")?);
    match copy_index(&own, &index.0) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => drop(read(staged()?.args(["read-tree", "HEAD"]), "read-tree")?),
        Err(e) => return Err(Error::Failed(format!("copy {}: {e}", own.display()))),
    }
    read(staged()?.args(["add", "--all", "--", ":/"]), "add")?;
    let tree = read(staged()?.arg("write-tree"), "write-tree")?;
    let head = read(query(&wt.path)?.args(["rev-parse", "HEAD"]), "rev-parse HEAD")?;
    let message = format!("krowk: final state of worktree {}", wt.hex);
    // The person's identity when git has one, krowk's when it has none.
    let commit = |identity: &[&str]| read(git(&wt.path)?.args(identity).args(["commit-tree", &tree, "-p", &head, "-m", &message]), "commit-tree");
    let commit = commit(&[]).or_else(|_| commit(&["-c", "user.name=krowk", "-c", "user.email=krowk@localhost"]))?;
    read(git(&wt.path)?.args(["update-ref", "-m", &message, &format!("{SNAPSHOTS}{}", wt.hex), &commit]), "update-ref")?;
    Ok(commit)
}

/// The paths that differ from `wt`'s base to `snapshot`, by path: the
/// files, and the gitlinks (submodules, or repositories added inside it).
fn changed(wt: &Worktree, snapshot: &str) -> Result<(Vec<String>, Vec<String>), Error> {
    let raw = read(query(&wt.path)?.args(["diff-tree", "-r", "-z", "--raw", "--no-renames", "--ignore-submodules=none", &wt.base, snapshot]), "diff-tree")?;
    let (mut files, mut links) = (Vec::new(), Vec::new());
    let mut fields = raw.split('\0');
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let mut modes = meta.trim_start_matches(':').split(' ');
        let gitlink = modes.next() == Some("160000") || modes.next() == Some("160000");
        if gitlink { links.push(path.to_string()) } else { files.push(path.to_string()) }
    }
    Ok((files, links))
}

/// The patch from `wt`'s base to `snapshot`, written to `to` as git wrote
/// it (a file's bytes need not be UTF-8): plumbing, so no config of the
/// person's changes its shape, and the prefixes `git apply -p1` expects.
fn diff(wt: &Worktree, snapshot: &str, to: &Path) -> Result<(), Error> {
    let file = std::fs::File::create(to).map_err(|e| Error::Failed(format!("create {}: {e}", to.display())))?;
    let out = query(&wt.path)?
        .args(["diff-tree", "-r", "-p", "--binary", "--full-index", "--no-renames", "--no-ext-diff", "--no-textconv", "--no-color", "--src-prefix=a/", "--dst-prefix=b/", &wt.base, snapshot])
        .stdout(file)
        .output()
        .map_err(|e| Error::Failed(format!("git diff-tree: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("git diff-tree: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

/// `git apply` of `patch` at `top` with `extra`: whether it applied (or,
/// with `--check`, would).
fn git_apply(top: &Path, patch: &Path, extra: &[&str]) -> Result<bool, Error> {
    let out = git(top)?.args(["apply", "--whitespace=nowarn"]).args(extra).arg(patch).output().map_err(|e| Error::Failed(format!("git apply: {e}")))?;
    Ok(out.status.success())
}

/// `path` as a pattern `git apply --include` matches only it by.
fn glob_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo, with_submodules};
    use super::super::{create, finish_into, prepare, Prepare};
    use super::*;
    use crate::instances::WorktreesConfig;

    /// A child that commits one change and leaves another uncommitted: both
    /// land in the parent's working tree as uncommitted changes, the
    /// parent's staged change, index and HEAD as they were; the worktree
    /// and branch are gone, the snapshot kept.
    #[test]
    fn wt13_a_commit_and_an_uncommitted_change_both_apply_to_the_working_tree_only() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt13-apply");
        std::fs::write(main.join("staged.txt"), "staged\n").unwrap();
        git_ok(&main, &["add", "staged.txt"]);
        let w = create(&main, &root, "s1").unwrap();
        std::fs::write(w.path.join("a.txt"), "committed\n").unwrap();
        git_ok(&w.path, &["commit", "-q", "-am", "child"]);
        std::fs::write(w.path.join("new.txt"), "uncommitted\n").unwrap();
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let cached = git_ok(&main, &["diff", "--cached"]);
        let index = std::fs::read(main.join(".git/index")).unwrap();

        let applied = finish_into(&w, &main).unwrap().expect("changed");
        assert_eq!(applied, Applied::Applied(vec!["a.txt".into(), "new.txt".into()]));
        assert_eq!(applied.note(&w).as_deref(), Some("Changes applied to your working tree: a.txt, new.txt"));
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "committed\n");
        assert_eq!(std::fs::read_to_string(main.join("new.txt")).unwrap(), "uncommitted\n");
        assert_eq!((git_ok(&main, &["rev-parse", "HEAD"]), git_ok(&main, &["diff", "--cached"])), (head, cached), "HEAD and the index as they were");
        assert_eq!(std::fs::read(main.join(".git/index")).unwrap(), index, "the index file untouched");
        assert_eq!(git_ok(&main, &["status", "--porcelain"]), "M a.txt\nA  staged.txt\n?? new.txt", "a.txt modified, unstaged (the output trimmed)");
        assert!(!w.path.exists());
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        let snapshot = format!("{SNAPSHOTS}{}", w.hex);
        assert_eq!(git_ok(&main, &["show", &format!("{snapshot}:new.txt")]), "uncommitted");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two children changing different files both apply; of two changing
    /// the same line, the first applies and the second is kept, its note
    /// naming the file, the parent's files as the first left them.
    #[test]
    fn wt13_siblings_apply_one_after_the_other_and_a_conflict_is_kept() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt13-siblings");
        let children: Vec<_> = (0..4).map(|i| create(&main, &root, &format!("s{i}")).unwrap()).collect();
        std::fs::write(children[0].path.join("one.txt"), "one\n").unwrap();
        std::fs::write(children[1].path.join("two.txt"), "two\n").unwrap();
        std::fs::write(children[2].path.join("a.txt"), "first\n").unwrap();
        std::fs::write(children[3].path.join("a.txt"), "second\n").unwrap();
        // Finished at once, as a response's subagents are.
        let results: Vec<Applied> = std::thread::scope(|s| {
            let handles: Vec<_> = children[..2].iter().map(|w| s.spawn(|| finish_into(w, &main).unwrap().unwrap())).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results, [Applied::Applied(vec!["one.txt".into()]), Applied::Applied(vec!["two.txt".into()])]);
        assert_eq!(finish_into(&children[2], &main).unwrap(), Some(Applied::Applied(vec!["a.txt".into()])));
        let kept = finish_into(&children[3], &main).unwrap().unwrap();
        assert_eq!(kept, Applied::Conflicts(vec!["a.txt".into()]));
        let w = &children[3];
        assert_eq!(kept.note(w).unwrap(), format!("Changes not applied (conflicts in a.txt). Worktree: {}, branch {}", w.path.display(), w.branch()));
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "first\n");
        assert_eq!(std::fs::read_to_string(main.join("one.txt")).unwrap() + &std::fs::read_to_string(main.join("two.txt")).unwrap(), "one\ntwo\n");
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "second\n");
        assert_eq!(git_ok(&main, &["branch", "--list", "--format=%(refname:short)", "krowk/*"]), w.branch(), "only the conflicting one's branch is left");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A binary file changed, another added, and a file deleted all apply.
    #[test]
    fn wt13_binary_and_deleted_files_apply() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("wt13-binary");
        let bytes = |seed: u8| (0..=255u8).map(|b| b.wrapping_mul(seed) ^ 0x80).chain([0, 0xff, 0]).collect::<Vec<u8>>();
        std::fs::write(main.join("blob.bin"), bytes(3)).unwrap();
        std::fs::write(main.join("gone.txt"), "gone\n").unwrap();
        git_ok(&main, &["add", "blob.bin", "gone.txt"]);
        git_ok(&main, &["commit", "-q", "-m", "files"]);
        let w = create(&main, &root, "s1").unwrap();
        std::fs::write(w.path.join("blob.bin"), bytes(7)).unwrap();
        std::fs::write(w.path.join("new.bin"), bytes(11)).unwrap();
        git_ok(&w.path, &["rm", "-q", "gone.txt"]);
        assert_eq!(finish_into(&w, &main).unwrap(), Some(Applied::Applied(vec!["blob.bin".into(), "gone.txt".into(), "new.bin".into()])));
        assert_eq!(std::fs::read(main.join("blob.bin")).unwrap(), bytes(7));
        assert_eq!(std::fs::read(main.join("new.bin")).unwrap(), bytes(11));
        assert!(!main.join("gone.txt").exists());
        assert_eq!(git_ok(&main, &["diff", "--cached"]), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An edit inside a submodule is not applied: the worktree is kept, and
    /// the note names the submodule. An unchanged one is removed as before.
    #[test]
    fn wt13_a_submodule_change_is_not_applied_and_the_worktree_is_kept() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = with_submodules("wt13-submodules");
        let w = create(&main, &root, "s1").unwrap();
        prepare(&Prepare::new(&w, &WorktreesConfig::default()));
        assert_eq!(finish_into(&w, &main).unwrap(), None);
        let w = create(&main, &root, "s2").unwrap();
        prepare(&Prepare::new(&w, &WorktreesConfig::default()));
        std::fs::write(w.path.join("a.txt"), "top\n").unwrap();
        std::fs::write(w.path.join("mid/f"), "changed\n").unwrap();
        let kept = finish_into(&w, &main).unwrap().unwrap();
        assert_eq!(kept, Applied::Submodules(vec!["mid".into()]));
        assert!(kept.note(&w).unwrap().starts_with("Changes not applied (they change submodules, which krowk does not apply: mid). Worktree: "));
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "a\n", "nothing applied");
        assert_eq!(std::fs::read_to_string(w.path.join("mid/f")).unwrap(), "changed\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wt13_a_note_names_twenty_files_then_how_many_more() {
        let files: Vec<String> = (0..23).map(|i| format!("f{i}")).collect();
        let n = named(&files);
        assert!(n.starts_with("f0, f1, ") && n.ends_with(", f19 and 3 more"), "{n}");
        assert_eq!(glob_escape("a[1]*?.txt"), r"a\[1\]\*\?.txt");
    }
}
