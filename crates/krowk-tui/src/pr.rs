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
    Command::new("git")
        .args(["-c", "core.fsmonitor=false", "--no-optional-locks", "rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .stdin(Stdio::null())
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

/// Where a tool call says the agent is at work, when it says: the
/// directory a command ran in, a command's leading `cd`, the directory of
/// a file it writes. The agent's worktree, say, rather than where the
/// session started. Relative to where the session runs.
pub fn worked_in(tool: &str, input: &Value) -> Option<PathBuf> {
    let text = |k: &str| input.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(d) = text("cwd").or_else(|| text("workdir")) {
        return Some(d.into());
    }
    match tool {
        "Bash" | "bash" | "shell" => {
            let rest = text("command")?.trim_start().strip_prefix("cd ")?;
            let dir = rest.split(['&', ';', '|', '\n']).next()?.trim().trim_matches(['"', '\'']);
            // What only a shell could expand is not a directory to look in.
            (!dir.is_empty() && !dir.contains(['$', '`', '~', '*'])).then(|| dir.into())
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "write" | "str_replace" | "search_replace" => {
            let file = text("file_path").or_else(|| text("path")).or_else(|| text("notebook_path"))?;
            Path::new(file).parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf)
        }
        _ => None,
    }
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
        let at = |tool: &str, input: Value| worked_in(tool, &input).map(|p| p.display().to_string());
        assert_eq!(at("Bash", json!({"command": "cd ../krowk-cli-x && git status"})).as_deref(), Some("../krowk-cli-x"));
        assert_eq!(at("bash", json!({"command": "cd '/repo/wt'; cargo test"})).as_deref(), Some("/repo/wt"));
        assert_eq!(at("shell", json!({"command": "ls", "cwd": "/repo/wt"})).as_deref(), Some("/repo/wt"), "Codex says where");
        assert_eq!(at("Edit", json!({"file_path": "/repo/wt/src/app.rs"})).as_deref(), Some("/repo/wt/src"));
        assert_eq!(at("write", json!({"path": "src/app.rs"})).as_deref(), Some("src"));
        assert_eq!(at("write", json!({"path": "README.md"})), None, "where the session runs");
        assert_eq!(at("Bash", json!({"command": "git status"})), None);
        assert_eq!(at("Bash", json!({"command": "cd $(git rev-parse --show-toplevel) && ls"})), None, "only a shell knows");
        assert_eq!(at("Read", json!({"file_path": "/elsewhere/notes.md"})), None, "reading is not working there");
    }
}
