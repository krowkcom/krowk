//! The branch checked out and its pull request, for the status line: read
//! where the agent is at work, at start, again after every turn, since a
//! turn is what switches branches and opens, merges or readies a pull
//! request, and whenever the agent moves to another directory. No `gh`,
//! no sign-in, no branch or no pull request all read as no pull request.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub state: State,
    pub url: String,
}

/// The branch checked out in `dir`: none outside a repository or on a
/// detached head.
pub fn branch(dir: &Path) -> String {
    krowk_api::git::query(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| b != "HEAD")
        .unwrap_or_default()
}

/// The branch checked out in the first of `dirs` inside a repository, and
/// its pull request as `gh` knows it when `pr` asks for it. Blocking: run
/// it off the loop.
pub fn look(dirs: &[PathBuf], pr: bool) -> (String, Option<Pr>) {
    let Some((dir, branch)) = dirs.iter().find_map(|d| Some((d, branch(d))).filter(|(_, b)| !b.is_empty())) else { return (String::new(), None) };
    let found = if pr { of(dir, &branch) } else { None };
    (branch, found)
}

/// Where the agent is at work, followed call by call: the agent's
/// worktree, say, rather than where the session started.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Follow {
    /// Where the session runs: what a relative path is relative to.
    pub runs_in: Option<PathBuf>,
    /// Where Claude Code's shell is. It keeps its directory from one call
    /// to the next while that is inside the project, and is put back where
    /// the session runs once it leaves.
    shell_in: Option<PathBuf>,
    /// The directory, inside a repository, the agent was last at work in.
    pub works_in: Option<PathBuf>,
}

impl Follow {
    /// What a tool call says: the directory a command ran in, a command's
    /// leading `cd`, the directory of a file it writes. Only a directory in
    /// a repository moves it: a note written to `/tmp` does not take the
    /// status line off the worktree.
    pub fn call(&mut self, tool: &str, input: &Value) {
        let Some(runs_in) = self.runs_in.clone() else { return };
        let text = |k: &str| input.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let at = if let Some(d) = text("cwd").or_else(|| text("workdir")) {
            clean(&runs_in.join(d))
        } else {
            match tool {
                "Bash" | "bash" | "shell" => {
                    let Some(dir) = text("command").and_then(leading_cd) else { return };
                    // krowk's own shell and Codex's start each command where
                    // the session runs; Claude Code's goes on where it was.
                    let base = if tool == "Bash" { self.shell_in.clone().unwrap_or_else(|| runs_in.clone()) } else { runs_in.clone() };
                    let to = clean(&base.join(dir));
                    if tool == "Bash" {
                        self.shell_in = to.starts_with(&runs_in).then(|| to.clone());
                    }
                    to
                }
                "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "write" | "str_replace" | "search_replace" => {
                    let Some(file) = text("file_path").or_else(|| text("path")).or_else(|| text("notebook_path")) else { return };
                    match clean(&runs_in.join(file)).parent() {
                        Some(d) => d.to_path_buf(),
                        None => return,
                    }
                }
                _ => return,
            }
        };
        // A `cd -P dir` or `cd build 2>/dev/null` read wrong names nothing.
        if at.is_dir() && at.ancestors().any(|a| a.join(".git").exists()) {
            self.works_in = Some(at);
        }
    }

    /// A turn starts where the session runs: Claude Code's process, and its
    /// shell, may be new (another model, another mode, a resume).
    pub fn turn_started(&mut self) {
        self.shell_in = None;
    }
}

/// The directory a command starts with changing to, when a person could
/// read it without a shell.
fn leading_cd(command: &str) -> Option<&str> {
    let rest = command.trim_start().strip_prefix("cd ")?;
    let dir = rest.split(['&', ';', '|', '\n']).next()?.trim().trim_matches(['"', '\'']);
    (!dir.is_empty() && !dir.contains(['$', '`', '~', '*'])).then_some(dir)
}

/// `a/b/../c` as `a/c`, without asking the filesystem: a worktree beside
/// the project is not inside it.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn of(dir: &Path, branch: &str) -> Option<Pr> {
    let out = Command::new("gh")
        .args(["pr", "view", branch, "--json", "number,state,isDraft,url"])
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    parse(&serde_json::from_slice(&out.stdout).ok()?)
}

fn parse(v: &Value) -> Option<Pr> {
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?;
    if !url.starts_with("https://") {
        return None;
    }
    let state = match (v.get("state")?.as_str()?, v.get("isDraft").and_then(Value::as_bool).unwrap_or(false)) {
        ("MERGED", _) => State::Merged,
        ("CLOSED", _) => State::Closed,
        (_, true) => State::Draft,
        _ => State::Open,
    };
    Some(Pr { number, state, url: url.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gh_s_answer_reads_as_the_pull_request_and_its_state() {
        let pr = |state: &str, draft: bool| parse(&json!({"number": 133, "state": state, "isDraft": draft, "url": "https://github.com/krowkcom/krowk-cli/pull/133"}));
        assert_eq!(pr("OPEN", false).map(|p| (p.number, p.state)), Some((133, State::Open)));
        assert_eq!(pr("OPEN", true).map(|p| p.state), Some(State::Draft));
        assert_eq!(pr("MERGED", false).map(|p| p.state), Some(State::Merged));
        assert_eq!(pr("CLOSED", false).map(|p| p.state), Some(State::Closed));
        assert_eq!(parse(&json!({"number": 1, "state": "OPEN", "url": "javascript:x"})), None, "only a web link");
        assert_eq!(parse(&json!({})), None);
    }

    #[test]
    fn a_tool_call_says_where_the_agent_is_at_work() {
        // A project, a worktree beside it, and a directory in neither.
        let base = std::env::temp_dir().join(format!("krowk-pr-follow-{}", std::process::id()));
        let (main, wt, tmp) = (base.join("main"), base.join("wt"), base.join("tmp"));
        for d in [main.join(".git"), main.join("crates/tui"), wt.join("crates/tui"), tmp.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt").unwrap();
        let mut f = Follow { runs_in: Some(main.clone()), ..Follow::default() };
        let call = |f: &mut Follow, tool: &str, input: Value| {
            f.call(tool, &input);
            f.works_in.clone()
        };
        assert_eq!(call(&mut f, "Read", json!({"file_path": wt.join("README.md")})), None, "reading is not working there");
        assert_eq!(call(&mut f, "Bash", json!({"command": "git status"})), None, "a command that does not say");
        assert_eq!(call(&mut f, "Bash", json!({"command": "cd ../wt && git status"})), Some(wt.clone()), "beside the project, a relative cd");
        // Out of the project, Claude Code's shell went back to it.
        assert_eq!(call(&mut f, "Bash", json!({"command": "cd crates/tui && cargo test"})), Some(main.join("crates/tui")));
        // Inside it, the shell stays where it went.
        assert_eq!(call(&mut f, "Bash", json!({"command": "cd .. && ls"})), Some(main.join("crates")));
        f.turn_started();
        assert_eq!(call(&mut f, "Bash", json!({"command": "cd crates && ls"})), Some(main.join("crates")), "a turn's shell starts where the session runs");
        assert_eq!(call(&mut f, "bash", json!({"command": "cd crates/tui && ls"})), Some(main.join("crates/tui")), "krowk's own shell starts where the session runs");
        assert_eq!(call(&mut f, "shell", json!({"command": "ls", "cwd": wt.join("crates/tui")})), Some(wt.join("crates/tui")), "Codex says where");
        assert_eq!(call(&mut f, "Write", json!({"file_path": tmp.join("notes.txt")})), Some(wt.join("crates/tui")), "outside any repository: still the worktree");
        assert_eq!(call(&mut f, "Bash", json!({"command": format!("cd {} && ls", tmp.display())})), Some(wt.join("crates/tui")));
        assert_eq!(call(&mut f, "write", json!({"path": "crates/tui/app.rs"})), Some(main.join("crates/tui")), "a relative path is where the session runs");
        assert_eq!(call(&mut f, "Edit", json!({"file_path": wt.join("crates/tui/app.rs")})), Some(wt.join("crates/tui")));
        assert_eq!(call(&mut f, "Bash", json!({"command": "cd $(git rev-parse --show-toplevel) && ls"})), Some(wt.join("crates/tui")), "only a shell knows");
        for unread in ["cd -", "cd -P crates", "cd crates 2>/dev/null && ls", "cd nowhere && ls"] {
            assert_eq!(call(&mut f, "Bash", json!({"command": unread})), Some(wt.join("crates/tui")), "{unread}: names no directory");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nothing_is_followed_before_the_session_says_where_it_runs() {
        let mut f = Follow::default();
        f.call("Edit", &json!({"file_path": std::env::current_dir().unwrap().join("src/pr.rs")}));
        assert_eq!(f, Follow::default());
    }
}
