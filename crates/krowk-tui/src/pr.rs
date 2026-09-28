//! The pull request of the branch checked out, for the status line: asked
//! of `gh` at start and again after every turn, since a turn is what opens,
//! merges or readies one. No `gh`, no sign-in, no branch or no pull request
//! all read as none.

use serde_json::Value;
use std::path::Path;
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

/// The pull request of the branch checked out in `dir`, as `gh` knows it.
/// Blocking: run it off the loop.
pub fn look(dir: &Path) -> Option<Pr> {
    let branch = branch(dir);
    if branch.is_empty() {
        return None;
    }
    let out = Command::new("gh")
        .args(["pr", "view", &branch, "--json", "number,state,isDraft,url"])
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
}
