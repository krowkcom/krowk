//! `krowk worktrees` (worktrees WT9), the built binary over worktrees made
//! through `krowk_harness::worktree`, in a stand-in home and data
//! directory: `list --json` flags a clean, a dirty and an ahead one;
//! `remove` refuses a dirty one with exit 4 and changes nothing, and
//! `--force` keeps a snapshot that brings the change back; `prune` clears
//! what `rm -rf` of one left.

#![cfg(all(feature = "harness", unix))]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-worktrees-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "data", "repo"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap() };
        git(&b.repo(), &["init", "-q", "-b", "main"]);
        std::fs::write(b.repo().join("a.txt"), "a\n").unwrap();
        git(&b.repo(), &["add", "a.txt"]);
        git(&b.repo(), &["commit", "-q", "-m", "one"]);
        b
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Where krowk keeps worktrees for this sandbox's `XDG_DATA_HOME`.
    fn worktrees(&self) -> PathBuf {
        self.root.join("data/krowk/worktrees")
    }

    fn create(&self, owner: &str) -> krowk_harness::worktree::Worktree {
        krowk_harness::worktree::create(&self.repo(), &self.worktrees(), owner).unwrap()
    }

    fn krowk(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(self.repo())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let o = self.krowk(args);
        assert!(o.status.success(), "krowk {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// git in `dir`, through krowk's own git, with none of this machine's
/// config: what it prints, trimmed.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn has_git() -> bool {
    Command::new("/usr/bin/env").args(["git", "--version"]).output().is_ok_and(|o| o.status.success())
}

/// The row `list --json` has for `hex`.
fn row<'a>(listed: &'a Value, hex: &str) -> &'a Value {
    listed["data"]["worktrees"].as_array().unwrap().iter().find(|r| r["hex"] == hex).unwrap_or_else(|| panic!("{hex} listed: {listed}"))
}

#[test]
fn wt9_list_json_flags_clean_dirty_and_ahead() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("list");
    let head = git(&b.repo(), &["rev-parse", "HEAD"]);
    let clean = b.create("s-clean");
    let dirty = b.create("s-dirty");
    std::fs::write(dirty.path.join("a.txt"), "changed\n").unwrap();
    let ahead = b.create("s-ahead");
    git(&ahead.path, &["commit", "-q", "--allow-empty", "-m", "two"]);

    let listed = b.json(&["worktrees", "--json"]);
    assert_eq!(listed["summary"], "3 worktrees (0 live)");
    let r = row(&listed, &clean.hex);
    assert_eq!((&r["dirty"], &r["ahead"], &r["live"], &r["missing"]), (&Value::Bool(false), &Value::from(0), &Value::Bool(false), &Value::Bool(false)));
    assert_eq!((r["base"].as_str(), r["session"].as_str(), r["branch"].as_str()), (Some(head.as_str()), Some("s-clean"), Some(clean.branch().as_str())));
    assert_eq!(r["path"].as_str(), clean.path.to_str());
    assert_eq!(r["repo"].as_str(), b.repo().to_str());
    for key in ["hex", "path", "repo", "branch", "base", "ahead", "dirty", "session", "created_ms", "age_seconds", "live", "missing"] {
        assert!(r.get(key).is_some(), "{key} on every row: {r}");
    }
    let r = row(&listed, &dirty.hex);
    assert_eq!((&r["dirty"], &r["ahead"]), (&Value::Bool(true), &Value::from(0)));
    let r = row(&listed, &ahead.hex);
    assert_eq!((&r["dirty"], &r["ahead"]), (&Value::Bool(false), &Value::from(1)));
    // `list` is the bare command's too, and a person gets a table.
    // Each row's age is counted afresh, and a second may pass between the two.
    let ageless = |v: &Value| {
        let mut v = v["data"].clone();
        for r in v["worktrees"].as_array_mut().into_iter().flatten() {
            r.as_object_mut().map(|o| o.remove("age_seconds"));
        }
        v
    };
    assert_eq!(ageless(&b.json(&["worktrees", "list", "--json"])), ageless(&listed));
    let human = String::from_utf8(b.krowk(&["worktrees", "--format", "human"]).stdout).unwrap();
    assert!(human.starts_with("WORKTREE") && human.contains(&dirty.hex) && human.ends_with("3 worktrees (0 live)\n"), "{human}");
}

#[test]
fn wt9_remove_refuses_a_dirty_worktree_and_force_keeps_a_snapshot() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("remove");
    let w = b.create("s1");
    std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
    std::fs::write(w.path.join("new.txt"), "new\n").unwrap();
    let state = || (git(&b.repo(), &["worktree", "list", "--porcelain"]), git(&w.path, &["status", "--porcelain"]), git(&b.repo(), &["for-each-ref"]));
    let before = state();

    let o = b.krowk(&["worktrees", "remove", &w.hex]);
    assert_eq!(o.status.code(), Some(4), "refused: {}", String::from_utf8_lossy(&o.stderr));
    let err: Value = serde_json::from_slice(&o.stderr).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stderr)));
    assert_eq!(err["error"]["error"], "worktree_has_changes");
    assert_eq!(err["error"]["details"]["dirty"], true);
    assert_eq!(state(), before, "nothing changed");

    let removed = b.json(&["worktrees", "remove", &w.hex, "--force", "--json"]);
    let snapshot = format!("refs/krowk/snapshots/{}", w.hex);
    assert_eq!(removed["data"]["snapshot"].as_str(), Some(snapshot.as_str()));
    assert_eq!(removed["data"]["branch"].as_str(), Some(w.branch().as_str()));
    assert!(!w.path.exists());
    assert_eq!(git(&b.repo(), &["branch", "--list", "--format=%(refname:short)", &w.branch()]), w.branch(), "the branch is kept");
    git(&b.repo(), &["worktree", "add", "-q", w.path.to_str().unwrap(), &w.branch()]);
    git(&w.path, &["stash", "apply", "-q", &snapshot]);
    assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(w.path.join("new.txt")).unwrap(), "new\n");

    let o = b.krowk(&["worktrees", "remove", "0badf00d"]);
    assert_eq!(o.status.code(), Some(2));
    // `--force` belongs to `remove` (and `host stop`), nowhere else here.
    assert_eq!(b.krowk(&["worktrees", "prune", "--force"]).status.code(), Some(1));
}

#[test]
fn wt9_prune_after_rm_rf_leaves_no_worktree_entry() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("prune");
    let w = b.create("crashed");
    std::fs::remove_dir_all(&w.path).unwrap();
    assert!(git(&b.repo(), &["worktree", "list"]).contains(&w.hex), "git still lists it, locked");
    let pruned = b.json(&["worktrees", "prune", "--json"]);
    assert_eq!(pruned["data"]["registrations"][0].as_str(), w.path.to_str(), "{pruned}");
    assert!(!git(&b.repo(), &["worktree", "list"]).contains(&w.hex));
    assert_eq!(b.json(&["worktrees", "--json"])["data"]["worktrees"], Value::Array(Vec::new()));
}

/// `apply` brings a kept worktree's commit and uncommitted change into a
/// target checkout (`--to`, the person's own worktree here) as uncommitted
/// changes, then removes it and its branch; while a live session holds
/// one it refuses with exit 4 and changes nothing; by default it applies
/// to the main checkout, and a conflict there is refused, naming the file.
#[test]
fn wt13_apply_brings_a_kept_worktree_into_a_checkout_and_refuses_while_live() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("apply");
    let w = b.create("s1");
    std::fs::write(w.path.join("a.txt"), "committed\n").unwrap();
    git(&w.path, &["commit", "-q", "-am", "child"]);
    std::fs::write(w.path.join("new.txt"), "uncommitted\n").unwrap();
    let target = b.root.join("target");
    git(&b.repo(), &["worktree", "add", "-q", "-b", "mine", target.to_str().unwrap()]);
    let head = git(&target, &["rev-parse", "HEAD"]);

    let applied = b.json(&["worktrees", "apply", &w.hex, "--to", target.to_str().unwrap(), "--json"]);
    assert_eq!(applied["data"]["files"], serde_json::json!(["a.txt", "new.txt"]), "{applied}");
    assert_eq!(applied["data"]["to"].as_str(), target.to_str());
    assert_eq!(applied["data"]["snapshot"].as_str(), Some(format!("refs/krowk/snapshots/{}", w.hex).as_str()));
    assert_eq!(std::fs::read_to_string(target.join("a.txt")).unwrap(), "committed\n");
    assert_eq!(std::fs::read_to_string(target.join("new.txt")).unwrap(), "uncommitted\n");
    assert_eq!((git(&target, &["rev-parse", "HEAD"]), git(&target, &["diff", "--cached"])), (head, String::new()));
    assert_eq!(std::fs::read_to_string(b.repo().join("a.txt")).unwrap(), "a\n", "the main checkout untouched");
    assert!(!w.path.exists());
    assert_eq!(git(&b.repo(), &["branch", "--list", "krowk/*"]), "");

    // Held by a live session: refused, nothing changed.
    let (live, held) = krowk_harness::worktree::create_held(&b.repo(), &b.worktrees(), "s2").unwrap();
    std::fs::write(live.path.join("a.txt"), "live\n").unwrap();
    let o = b.krowk(&["worktrees", "apply", &live.hex]);
    assert_eq!(o.status.code(), Some(4), "{}", String::from_utf8_lossy(&o.stderr));
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(err["error"]["error"], "worktree_live");
    assert_eq!(std::fs::read_to_string(b.repo().join("a.txt")).unwrap(), "a\n");
    drop(held);

    // The main checkout by default; a file it changed too conflicts.
    std::fs::write(b.repo().join("a.txt"), "mine\n").unwrap();
    let o = b.krowk(&["worktrees", "apply", &live.hex]);
    assert_eq!(o.status.code(), Some(4), "{}", String::from_utf8_lossy(&o.stderr));
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!((&err["error"]["error"], &err["error"]["details"]["files"]), (&Value::from("worktree_conflicts"), &serde_json::json!(["a.txt"])));
    assert!(live.path.exists());
    std::fs::write(b.repo().join("a.txt"), "a\n").unwrap();
    let human = b.krowk(&["worktrees", "apply", live.path.to_str().unwrap(), "--format", "human"]);
    assert!(human.status.success(), "{}", String::from_utf8_lossy(&human.stderr));
    assert!(String::from_utf8_lossy(&human.stdout).starts_with("applied  a.txt\nApplied 1 changed file from "));
    assert_eq!(std::fs::read_to_string(b.repo().join("a.txt")).unwrap(), "live\n");
    // A checkout of another repository is refused.
    let w = b.create("s3");
    std::fs::write(w.path.join("a.txt"), "elsewhere\n").unwrap();
    let other = b.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    git(&other, &["commit", "-q", "--allow-empty", "-m", "other"]);
    let o = b.krowk(&["worktrees", "apply", &w.hex, "--to", other.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stderr));
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(err["error"]["error"], "worktree_other_repository");
    assert!(!other.join("a.txt").exists() && w.path.is_dir());
    // `--to` belongs to `apply`.
    assert_eq!(b.krowk(&["worktrees", "prune", "--to", "x"]).status.code(), Some(1));
}
