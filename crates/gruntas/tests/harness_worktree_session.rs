//! `gruntas -p --worktree` (worktrees WT6), the built binary against the
//! fake `claude` (Claude Code) on PATH, in a real repository with a stand-in home and
//! data directory: a session that makes a file makes it in a new worktree,
//! not the checkout, and the worktree is kept and named, in the result and
//! on stderr; a session that changes nothing leaves no worktree or branch;
//! a resumed one runs in its worktree again, and one whose worktree is gone
//! is refused; outside a repository the flag is refused; `--help` lists it.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A real repository, and the fake `claude` first on PATH, signed in to
    /// the default account (`~/.claude`): a prompt that asks for a file
    /// makes NEW.md where Claude Code runs, as its Write would, and any
    /// other is only answered. The repository and the directory outside it
    /// are trusted for good, as Claude Code runs in no other.
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-worktree-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home/.claude", "data", "run", "repo", "elsewhere", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("home/.claude/fake-login"), "").unwrap();
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/fake-claude"), root.join("bin/claude")).unwrap();
        std::fs::write(root.join("scenario.jsonl"), SCENARIO).unwrap();
        let b = Sandbox { root: root.canonicalize().unwrap() };
        std::fs::create_dir_all(b.root.join("home/.gruntas")).unwrap();
        std::fs::write(b.root.join("home/.gruntas/trusted.json"), json!({"directories": [b.repo(), b.root.join("elsewhere")]}).to_string()).unwrap();
        git(&b.repo(), &["init", "-q", "-b", "main"]);
        std::fs::write(b.repo().join("README.md"), "# gruntas\n").unwrap();
        git(&b.repo(), &["add", "README.md"]);
        git(&b.repo(), &["commit", "-q", "-m", "one"]);
        b
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Where gruntas keeps worktrees for this sandbox's `XDG_DATA_HOME`.
    fn worktrees(&self) -> PathBuf {
        self.root.join("data/gruntas/worktrees")
    }

    fn gruntas_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gruntas"))
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .env("FAKE_CLAUDE_SCENARIO", self.root.join("scenario.jsonl"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // Nothing above the sandbox is a repository gruntas could find.
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `gruntas -p` in the repository, the mode chosen so no sandbox is
    /// needed: what it printed, stdout and stderr, having succeeded.
    fn prompt(&self, args: &[&str]) -> (String, String) {
        let args: Vec<&str> = ["-p", "--model", "claude/sonnet", "--permission-mode", "acceptEdits"].into_iter().chain(args.iter().copied()).collect();
        let o = self.gruntas_in(&self.repo(), &args);
        let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
        assert!(o.status.success(), "gruntas {args:?}: {out}{err}");
        (out, err)
    }

    /// The directory the session `id` recorded it started in.
    fn session_cwd(&self, id: &str) -> String {
        let env = |k: &str| match k {
            "HOME" => self.root.join("home").display().to_string(),
            _ => String::new(),
        };
        let events = gruntas_harness::log::sessions_dir(&env).unwrap().join(id).join("events.jsonl");
        let first: Value = serde_json::from_str(std::fs::read_to_string(events).unwrap().lines().next().unwrap()).unwrap();
        first["cwd"].as_str().unwrap().to_string()
    }

    /// The worktree directories under the root, of every repository.
    fn worktree_dirs(&self) -> Vec<PathBuf> {
        let Ok(repos) = std::fs::read_dir(self.worktrees()) else { return Vec::new() };
        // A worktree's is named by 8 hex digits; the template and its
        // scratch directory beside them stay (WT10, WT11).
        let hex = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.len() == 8 && n.bytes().all(|b| b.is_ascii_hexdigit()));
        repos.flatten().filter(|r| r.path().is_dir()).flat_map(|r| std::fs::read_dir(r.path()).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir() && hex(p)).collect::<Vec<_>>()).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// git in `dir`, with none of this machine's config: what it prints.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = gruntas_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn has_git() -> bool {
    Command::new("/usr/bin/env").args(["git", "--version"]).output().is_ok_and(|o| o.status.success())
}

/// The fake's scenario: asked for a file, its Write makes NEW.md in the
/// directory Claude Code runs in (`@touch`); asked anything else, a plain
/// answer.
const SCENARIO: &str = r#"@when make a file
{"type":"assistant","message":{"id":"msg_w","model":"{{MODEL}}","role":"assistant","content":[{"type":"tool_use","id":"toolu_w","name":"Write","input":{"file_path":"NEW.md","content":"from the session\n"}}],"usage":{"input_tokens":3,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"{{SESSION}}"}
@touch NEW.md
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_w","type":"tool_result","content":"File created successfully at: NEW.md"}]},"parent_tool_use_id":null,"session_id":"{{SESSION}}"}
{"type":"assistant","message":{"id":"msg_done","model":"{{MODEL}}","role":"assistant","content":[{"type":"text","text":"made it."}],"usage":{"input_tokens":3,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"{{SESSION}}"}
{"type":"result","subtype":"success","is_error":false,"result":"made it.","session_id":"{{SESSION}}"}
"#;

/// What the fake logged: every argv, and each line in and out.
fn fake_log(b: &Sandbox) -> String {
    std::fs::read_to_string(b.root.join("fake.log")).unwrap_or_default()
}

#[test]
fn wt6_a_worktree_session_makes_its_file_in_the_worktree_which_is_kept_and_named() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("kept");
    let (out, err) = b.prompt(&["--worktree", "--output-format", "json", "make a file"]);
    let result: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(result["status"], "completed", "{result}");
    let wt = &result["worktree"];
    let path = PathBuf::from(wt["path"].as_str().unwrap_or_else(|| panic!("the kept worktree is named: {result}")));
    let hex = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!((wt["branch"].as_str(), &wt["commits"], &wt["uncommittedChanges"]), (Some(format!("gruntas/{hex}").as_str()), &json!(0), &json!(true)));
    assert!(path.starts_with(b.worktrees()), "under gruntas's worktrees: {}", path.display());
    assert!(path.join("NEW.md").is_file(), "made in the worktree");
    assert!(!b.repo().join("NEW.md").exists(), "not in the checkout");
    assert!(!err.contains("Worktree kept"), "JSON names it in the result alone: {err}");
    // The session ran there, by the id the worktree was made for.
    let session = result["sessionId"].as_str().unwrap();
    assert_eq!(b.session_cwd(session), path.display().to_string());
    assert_eq!(git(&b.repo(), &["branch", "--list", "--format=%(refname:short)", "gruntas/*"]), format!("gruntas/{hex}"));
    let listed = b.gruntas_in(&b.repo(), &["worktrees", "--json"]);
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let row = listed["data"]["worktrees"].as_array().unwrap().iter().find(|r| r["hex"] == hex.as_str()).unwrap_or_else(|| panic!("{listed}"));
    assert_eq!((row["session"].as_str(), &row["live"]), (Some(session), &json!(false)));

    // Resumed, it runs in its worktree again, and is named again on the
    // way out: on stderr, the answer being text.
    let (_, err) = b.prompt(&["--resume", session, "anything else?"]);
    assert!(err.contains(&format!("Worktree kept: {} (branch gruntas/{hex}) — `gruntas worktrees apply {hex}` brings its changes into your checkout", path.display())), "{err}");
    assert!(path.join("NEW.md").is_file());

    // A new session and a resumed one are not the same thing.
    let o = b.gruntas_in(&b.repo(), &["-p", "--worktree", "--resume", session, "hi"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--worktree starts a new session"), "{}", String::from_utf8_lossy(&o.stderr));

    // Gone, it is refused, naming what lists the ones that remain.
    std::fs::remove_dir_all(&path).unwrap();
    let o = b.gruntas_in(&b.repo(), &["-p", "--model", "claude/sonnet", "--resume", session, "hi"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{said}");
    assert!(said.contains("worktree_missing") && said.contains("gruntas worktrees"), "{said}");
}

#[test]
fn wt6_a_worktree_session_that_changes_nothing_leaves_nothing() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("unchanged");
    for format in ["text", "stream-json"] {
        let (out, err) = b.prompt(&["--worktree", "--output-format", format, "say hello"]);
        assert!(!err.contains("Worktree kept"), "{err}");
        if format == "stream-json" {
            let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
            assert_eq!(last["type"], "result", "the result is still last: {out}");
            assert!(last.get("worktree").is_none(), "{last}");
        }
        assert_eq!(b.worktree_dirs(), Vec::<PathBuf>::new(), "no worktree left");
        assert_eq!(git(&b.repo(), &["branch", "--list", "gruntas/*"]), "");
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
    let b = Sandbox::new("no-repo");
    let o = b.gruntas_in(&b.root.join("elsewhere"), &["-p", "--model", "claude/sonnet", "--worktree", "make a file"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{said}");
    assert!(said.contains("not_a_repository") && said.contains("this directory is in none"), "{said}");
    assert!(!fake_log(&b).contains("argv -p"), "Claude Code was never started");
    assert!(!b.root.join("elsewhere/NEW.md").exists());
}

#[test]
fn worktrees_default_starts_a_new_session_in_its_own_worktree_without_the_flag() {
    if !has_git() {
        return;
    }
    let b = Sandbox::new("default");
    std::fs::create_dir_all(b.root.join("home/.gruntas")).unwrap();
    std::fs::write(b.root.join("home/.gruntas/config.json"), r#"{"worktrees": {"default": true}}"#).unwrap();
    let (out, _) = b.prompt(&["--output-format", "json", "make a file"]);
    let result: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let path = PathBuf::from(result["worktree"]["path"].as_str().unwrap_or_else(|| panic!("the kept worktree is named: {result}")));
    assert!(path.starts_with(b.worktrees()) && path.join("NEW.md").is_file(), "{result}");
    assert!(!b.repo().join("NEW.md").exists(), "not in the checkout");

    // `--worktree=false` runs one in place.
    b.prompt(&["--worktree=false", "make a file"]);
    assert!(b.repo().join("NEW.md").is_file());
    std::fs::remove_file(b.repo().join("NEW.md")).unwrap();

    // A repository's own says no before the person's yes. (It is read only
    // once the repository is trusted; Claude Code runs in no other, so an
    // untrusted one's is never reached by a session.)
    std::fs::create_dir_all(b.repo().join(".gruntas")).unwrap();
    std::fs::write(b.repo().join(".gruntas/config.json"), r#"{"worktrees": {"default": false}}"#).unwrap();
    // A file that can run code changed since the repository was trusted, so
    // it is trusted again, as a person who looked at the change would.
    let o = b.gruntas_in(&b.repo(), &["trust"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (out, _) = b.prompt(&["--output-format", "json", "make a file"]);
    assert!(!out.contains("\"worktree\""), "the trusted repository's no is read: {out}");
    assert!(b.repo().join("NEW.md").is_file());

    // Outside a repository there is nothing to make one of: the session
    // runs where it is, where `--worktree` would be refused.
    let o = b.gruntas_in(&b.root.join("elsewhere"), &["-p", "--model", "claude/sonnet", "--permission-mode", "acceptEdits", "make a file"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(b.root.join("elsewhere/NEW.md").is_file());
}

#[test]
fn wt6_help_lists_the_flag_and_sessions_keeps_its_own() {
    let b = Sandbox::new("help");
    let o = b.gruntas_in(&b.repo(), &["--help"]);
    let help = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = help.lines().filter(|l| l.contains("--worktree")).collect();
    assert_eq!(lines.len(), 1, "{help}");
    assert!(lines[0].contains("start one in a worktree"), "{}", lines[0]);
    // `gruntas help flags` has it among the agent's flags, whole.
    let o = b.gruntas_in(&b.repo(), &["help", "flags"]);
    let flags = String::from_utf8_lossy(&o.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flags.contains("--worktree With -p: start the session in a new git worktree of this repository"), "{flags}");
    // `gruntas sessions --worktree <path>` still filters the listing.
    let o = b.gruntas_in(&b.repo(), &["sessions", "--worktree", "/nowhere", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // Anywhere else, it is named as the flag of what takes it.
    let o = b.gruntas_in(&b.repo(), &["worktrees", "list", "--worktree"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("`gruntas -p`"), "{}", String::from_utf8_lossy(&o.stderr));
}
