//! `krowk -p --worktree` (worktrees WT6), the built binary against the
//! stand-in Anthropic API, in a real repository with a stand-in home and
//! data directory: a session that makes a file makes it in a new worktree,
//! not the checkout, and the worktree is kept and named, in the result and
//! on stderr; a session that changes nothing leaves no worktree or branch;
//! a resumed one runs in its worktree again, and one whose worktree is gone
//! is refused; outside a repository the flag is refused; `--help` lists it.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
    url: String,
}

impl Sandbox {
    fn new(name: &str, url: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-worktree-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "data", "run", "repo", "elsewhere"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap(), url: url.into() };
        git(&b.repo(), &["init", "-q", "-b", "main"]);
        std::fs::write(b.repo().join("README.md"), "# krowk\n").unwrap();
        git(&b.repo(), &["add", "README.md"]);
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

    fn krowk_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("ANTHROPIC_API_KEY", "sk-test")
            .env("ANTHROPIC_BASE_URL", &self.url)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // Nothing above the sandbox is a repository krowk could find.
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `krowk -p` in the repository, the mode chosen so no sandbox is
    /// needed: what it printed, stdout and stderr, having succeeded.
    fn prompt(&self, args: &[&str]) -> (String, String) {
        let args: Vec<&str> = ["-p", "--model", "anthropic/claude-sonnet-4-6", "--permission-mode", "acceptEdits"].into_iter().chain(args.iter().copied()).collect();
        let o = self.krowk_in(&self.repo(), &args);
        let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
        assert!(o.status.success(), "krowk {args:?}: {out}{err}");
        (out, err)
    }

    /// The directory the session `id` recorded it started in.
    fn session_cwd(&self, id: &str) -> String {
        let env = |k: &str| match k {
            "HOME" => self.root.join("home").display().to_string(),
            _ => String::new(),
        };
        let events = krowk_harness::log::sessions_dir(&env).unwrap().join(id).join("events.jsonl");
        let first: Value = serde_json::from_str(std::fs::read_to_string(events).unwrap().lines().next().unwrap()).unwrap();
        first["cwd"].as_str().unwrap().to_string()
    }

    /// The worktree directories under the root, of every repository.
    fn worktree_dirs(&self) -> Vec<PathBuf> {
        let Ok(repos) = std::fs::read_dir(self.worktrees()) else { return Vec::new() };
        repos.flatten().filter(|r| r.path().is_dir()).flat_map(|r| std::fs::read_dir(r.path()).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>()).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// git in `dir`, with none of this machine's config: what it prints.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn has_git() -> bool {
    Command::new("/usr/bin/env").args(["git", "--version"]).output().is_ok_and(|o| o.status.success())
}

/// A model that writes NEW.md when the prompt asks for a file, and only
/// answers otherwise.
fn script(body: &Value, _: usize) -> mock::Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let answered = messages.last().and_then(|m| m["content"].as_array().cloned()).is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    if answered {
        return mock::Reply::sse(&mock::text_stream("made it."));
    }
    if messages.last().is_some_and(|m| m.to_string().contains("make a file")) {
        return mock::Reply::sse(&mock::tool_use("toolu_w", "write", &json!({"path": "NEW.md", "content": "from the session\n"})));
    }
    mock::Reply::sse(&mock::text_stream("nothing to do."))
}

#[test]
fn wt6_a_worktree_session_makes_its_file_in_the_worktree_which_is_kept_and_named() {
    if !has_git() {
        return;
    }
    let m = mock::serve(script);
    let b = Sandbox::new("kept", &m.url);
    let (out, err) = b.prompt(&["--worktree", "--output-format", "json", "make a file"]);
    let result: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(result["status"], "completed", "{result}");
    let wt = &result["worktree"];
    let path = PathBuf::from(wt["path"].as_str().unwrap_or_else(|| panic!("the kept worktree is named: {result}")));
    let hex = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!((wt["branch"].as_str(), &wt["commits"], &wt["uncommittedChanges"]), (Some(format!("krowk/{hex}").as_str()), &json!(0), &json!(true)));
    assert!(path.starts_with(b.worktrees()), "under krowk's worktrees: {}", path.display());
    assert_eq!(std::fs::read_to_string(path.join("NEW.md")).unwrap(), "from the session\n");
    assert!(!b.repo().join("NEW.md").exists(), "not in the checkout");
    assert!(!err.contains("Worktree kept"), "JSON names it in the result alone: {err}");
    // The session ran there, by the id the worktree was made for.
    let session = result["sessionId"].as_str().unwrap();
    assert_eq!(b.session_cwd(session), path.display().to_string());
    assert_eq!(git(&b.repo(), &["branch", "--list", "--format=%(refname:short)", "krowk/*"]), format!("krowk/{hex}"));
    let listed = b.krowk_in(&b.repo(), &["worktrees", "--json"]);
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let row = listed["data"]["worktrees"].as_array().unwrap().iter().find(|r| r["hex"] == hex.as_str()).unwrap_or_else(|| panic!("{listed}"));
    assert_eq!((row["session"].as_str(), &row["live"]), (Some(session), &json!(false)));

    // Resumed, it runs in its worktree again, and is named again on the
    // way out: on stderr, the answer being text.
    let (_, err) = b.prompt(&["--resume", session, "anything else?"]);
    assert!(err.contains(&format!("Worktree kept: {} (branch krowk/{hex})", path.display())), "{err}");
    assert!(path.join("NEW.md").is_file());

    // A new session and a resumed one are not the same thing.
    let o = b.krowk_in(&b.repo(), &["-p", "--worktree", "--resume", session, "hi"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--worktree starts a new session"), "{}", String::from_utf8_lossy(&o.stderr));

    // Gone, it is refused, naming what lists the ones that remain.
    std::fs::remove_dir_all(&path).unwrap();
    let o = b.krowk_in(&b.repo(), &["-p", "--model", "anthropic/claude-sonnet-4-6", "--resume", session, "hi"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{said}");
    assert!(said.contains("worktree_missing") && said.contains("krowk worktrees"), "{said}");
}

#[test]
fn wt6_a_worktree_session_that_changes_nothing_leaves_nothing() {
    if !has_git() {
        return;
    }
    let m = mock::serve(script);
    let b = Sandbox::new("unchanged", &m.url);
    for format in ["text", "stream-json"] {
        let (out, err) = b.prompt(&["--worktree", "--output-format", format, "say hello"]);
        assert!(!err.contains("Worktree kept"), "{err}");
        if format == "stream-json" {
            let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
            assert_eq!(last["type"], "result", "the result is still last: {out}");
            assert!(last.get("worktree").is_none(), "{last}");
        }
        assert_eq!(b.worktree_dirs(), Vec::<PathBuf>::new(), "no worktree left");
        assert_eq!(git(&b.repo(), &["branch", "--list", "krowk/*"]), "");
        assert_eq!(git(&b.repo(), &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
    }
    // stream-json names a kept one in its last line, the result.
    let (out, _) = b.prompt(&["--worktree", "--output-format", "stream-json", "make a file"]);
    let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "result");
    assert!(last["worktree"]["path"].as_str().is_some_and(|p| Path::new(p).join("NEW.md").is_file()), "{last}");
}

#[test]
fn wt6_worktree_outside_a_repository_is_refused() {
    if !has_git() {
        return;
    }
    let m = mock::serve(script);
    let b = Sandbox::new("no-repo", &m.url);
    let o = b.krowk_in(&b.root.join("elsewhere"), &["-p", "--model", "anthropic/claude-sonnet-4-6", "--worktree", "make a file"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{said}");
    assert!(said.contains("not_a_repository") && said.contains("this directory is in none"), "{said}");
    assert!(m.seen.lock().unwrap().is_empty(), "no model was called");
    assert!(!b.root.join("elsewhere/NEW.md").exists());
}

#[test]
fn wt6_help_lists_the_flag_and_sessions_keeps_its_own() {
    let m = mock::serve(script);
    let b = Sandbox::new("help", &m.url);
    let o = b.krowk_in(&b.repo(), &["--help"]);
    let help = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = help.lines().filter(|l| l.contains("--worktree")).collect();
    assert_eq!(lines.len(), 1, "{help}");
    assert!(lines[0].contains("start one in a worktree"), "{}", lines[0]);
    // `krowk help flags` has it among the agent's flags, whole.
    let o = b.krowk_in(&b.repo(), &["help", "flags"]);
    let flags = String::from_utf8_lossy(&o.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flags.contains("--worktree With -p, and in the TUI: start the session in a new git worktree of this repository"), "{flags}");
    // `krowk sessions --worktree <path>` still filters the listing.
    let o = b.krowk_in(&b.repo(), &["sessions", "--worktree", "/nowhere", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // Anywhere else, it is named as the flag of what takes it.
    let o = b.krowk_in(&b.repo(), &["worktrees", "list", "--worktree"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("`krowk -p` and the TUI"), "{}", String::from_utf8_lossy(&o.stderr));
}
