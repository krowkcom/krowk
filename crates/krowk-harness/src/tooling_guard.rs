//! The tooling guard: an agent that reaches past krowk for what krowk
//! does itself is refused, that one call, with a message naming krowk's way.
//!
//! - **A worktree by hand**: `git worktree add`, or Claude Code's
//!   `EnterWorktree` tool. A worktree krowk makes is locked to its session,
//!   readied (submodules, seeds, `worktrees.setup`), held against the
//!   machine's agent slots and finished when the work ends; one made by
//!   hand is none of that, and `krowk worktrees` never sees it.
//! - **A file sent to a paste site**: `gh gist create`, or `curl`, `wget`
//!   or `nc` naming one of `PASTE_HOSTS`. krowk publishes a file as an
//!   artifact, whose link unfurls and stays in the person's workspace.
//!
//! It runs with the paste guard (`crate::paste_guard`), on every backend
//! the same way, and like it holds in every mode: it steers the agent, not
//! the person, who can run any of these at their own prompt. Commands the
//! shell computes, or ones run through a script, are not seen.

use crate::permissions::rules;

/// Hosts that take a file and hand back a public link.
const PASTE_HOSTS: &[&str] = &[
    "0x0.st",
    "bashupload.com",
    "catbox.moe",
    "dpaste.com",
    "file.io",
    "ix.io",
    "paste.rs",
    "pastebin.com",
    "sprunge.us",
    "temp.sh",
    "termbin.com",
    "transfer.sh",
    "uguu.se",
];

/// What `git worktree add` and `EnterWorktree` are refused with.
pub const WORKTREE: &str = "krowk did not run this: krowk makes the worktrees agents work in, so each is locked to its session, readied and finished, and `krowk worktrees` lists it. \
     For work of its own, hand it to a subagent with `isolation: \"worktree\"`; for the whole session, the person starts krowk with `--worktree` (or sets `worktrees.default`). \
     A person can still make one by hand at their own prompt.";

/// What a paste-site upload is refused with.
pub const UPLOAD: &str = "krowk did not run this: files are published as krowk artifacts, not to a paste site. \
     Use the `publish` tool, or `krowk push FILE --json`, and paste the link or the `markdown` form it returns.";

/// Why `command` is not run, when it makes a worktree by hand or sends a
/// file to a paste site.
pub fn refusal(command: &str) -> Option<&'static str> {
    if !(command.contains("worktree") || command.contains("gist") || PASTE_HOSTS.iter().any(|h| command.contains(h))) {
        return None;
    }
    rules::split(command).commands.iter().find_map(|c| {
        let words: Vec<&str> = c.words.iter().map(String::as_str).collect();
        let at = words.iter().position(|w| !is_assignment(w) && !matches!(*w, "env" | "command" | "exec" | "nohup" | "time" | "sudo"))?;
        let rest = &words[at + 1..];
        match words[at].rsplit('/').next()? {
            "git" => worktree_add(rest).then_some(WORKTREE),
            "gh" => gist_create(rest).then_some(UPLOAD),
            "curl" | "wget" | "nc" | "ncat" | "netcat" => rest.iter().any(|w| names_paste_host(w)).then_some(UPLOAD),
            _ => None,
        }
    })
}

/// Whether git's arguments `args` are `[global options] worktree add`.
fn worktree_add(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        match *w {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    args.get(i) == Some(&"worktree") && args.get(i + 1) == Some(&"add")
}

/// Whether gh's arguments `args` are `[-R repo] gist create`.
fn gist_create(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        match *w {
            "-R" | "--repo" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    args.get(i) == Some(&"gist") && args.get(i + 1) == Some(&"create")
}

/// Whether `word` is a URL or host on one of `PASTE_HOSTS`.
fn names_paste_host(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    let rest = word.split_once("://").map_or(word.as_str(), |(_, r)| r);
    let host = rest.split(['/', ':', '?']).next().unwrap_or_default();
    PASTE_HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_made_by_hand_is_refused_however_git_is_called() {
        for c in [
            "git worktree add ../wt -b feature",
            "git -C /repo worktree add /tmp/wt",
            "cd /repo && GIT_DIR=.git git --no-pager worktree add wt",
            "/usr/bin/git -c core.hooksPath=/dev/null worktree add wt",
            "echo go; git worktree add wt HEAD",
        ] {
            assert_eq!(refusal(c), Some(WORKTREE), "{c}");
        }
        for c in ["git worktree list", "git worktree remove wt", "git status", "grep -r 'git worktree add' docs", "krowk worktrees", "echo git worktree add"] {
            assert_eq!(refusal(c), None, "{c}");
        }
    }

    #[test]
    fn a_file_sent_to_a_paste_site_is_refused_and_other_requests_are_not() {
        for c in [
            "gh gist create shot.png",
            "gh -R me/repo gist create --public log.txt",
            "curl -F 'file=@shot.png' https://0x0.st",
            "curl --upload-file ./log.txt https://transfer.sh/log.txt",
            "cat log | nc termbin.com 9999",
            "wget --post-file=log.txt https://paste.rs/",
            "curl -T log https://litterbox.catbox.moe/resources/internals/api.php",
        ] {
            assert_eq!(refusal(c), Some(UPLOAD), "{c}");
        }
        for c in [
            "gh gist list",
            "gh gist view abc",
            "curl -F 'file=@a.json' https://api.example.com/upload",
            "curl https://example.com/notpastebin.com",
            "krowk push shot.png --json",
            "echo transfer.sh",
        ] {
            assert_eq!(refusal(c), None, "{c}");
        }
    }
}
