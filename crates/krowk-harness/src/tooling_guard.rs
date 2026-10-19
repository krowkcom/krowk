//! The tooling guard: an agent that reaches past krowk for what krowk
//! does itself is refused, that one call, with a message naming krowk's way.
//!
//! - **A worktree by hand**: `git worktree add`, Claude Code's
//!   `EnterWorktree`, or its `Agent` with `isolation: "worktree"`. A
//!   worktree krowk makes is locked to its session, readied (submodules,
//!   seeds, `worktrees.setup`), held against the machine's agent slots and
//!   finished when the work ends; one made any other way is none of that,
//!   and `krowk worktrees` never sees it.
//! - **A file sent to a paste site**: `gh gist create`, an upload by
//!   `curl` or `wget` to one of `PASTE_HOSTS`, or `nc` to one. krowk
//!   publishes a file as an artifact, whose link unfurls and stays in the
//!   person's workspace. Reading from one is fine.
//!
//! It runs with the paste guard (`crate::paste_guard::guard`), on every
//! backend the same way, and like it holds in every mode: it steers the
//! agent, not the person, who can run any of these at their own prompt.
//! Heredoc bodies are text, not commands. A command is judged in every form
//! a deny rule is (`rules::forms`): behind `sudo`, `env`, `timeout` and the
//! like, and inside `bash -c`. One the shell computes, or one a script
//! runs, is not seen.

use crate::permissions::rules;
use serde_json::Value;

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

/// What a worktree made by hand is refused with.
pub const WORKTREE: &str = "krowk did not run this: krowk makes the worktrees agents work in, so each is locked to its session, readied and finished, and `krowk worktrees` lists it. \
     To work apart from the checkout, the person starts krowk with `--worktree` or sets `worktrees.default`; in krowk's own agent, a `subagent` call with `isolation: \"worktree\"` gives a child one. \
     Otherwise work where you are. A person can still make one by hand at their own prompt.";

/// What a paste-site upload is refused with.
pub const UPLOAD: &str = "krowk did not run this: files are published as krowk artifacts, not to a paste site. \
     Use the `publish` tool, or `krowk push FILE --json`, and paste the link or the `markdown` form it returns.";

/// A curl option that sends a file or data: the flag, or its short form
/// with the value attached.
const CURL_SENDS: &[&str] = &["-F", "--form", "--form-string", "-T", "--upload-file", "-d", "--data", "--data-binary", "--data-raw", "--data-urlencode", "--json"];

/// A wget option that sends a file or data.
const WGET_SENDS: &[&str] = &["--post-file", "--post-data", "--body-file", "--body-data"];

/// The options whose value is a local file to write or read, never where
/// the request goes.
const LOCAL_FILE: &[&str] = &["-o", "--output", "-O", "--output-document", "-a", "--append-output", "-D", "--dump-header", "-c", "--cookie-jar", "-K", "--config"];

/// Why `command` is not run, when it makes a worktree by hand or sends a
/// file to a paste site.
pub fn refusal(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    if !(lower.contains("worktree") || lower.contains("gist") || PASTE_HOSTS.iter().any(|h| lower.contains(h))) {
        return None;
    }
    let (rest, _) = crate::paste_guard::heredocs(command);
    rules::split(&rest).commands.iter().flat_map(rules::forms).find_map(|form| {
        let (program, args) = form.split_first()?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match program.rsplit('/').next()? {
            "git" => worktree_add(&args).then_some(WORKTREE),
            "gh" => gist_create(&args).then_some(UPLOAD),
            "curl" => (sends(&args, CURL_SENDS) && names_paste_host(&args)).then_some(UPLOAD),
            "wget" => ((sends(&args, WGET_SENDS) || args.iter().any(|a| a.starts_with("--method"))) && names_paste_host(&args)).then_some(UPLOAD),
            "nc" | "ncat" | "netcat" => names_paste_host(&args).then_some(UPLOAD),
            _ => None,
        }
    })
}

/// Why Claude Code's own tool `tool` is not run with `input`: a worktree
/// it would make itself.
pub fn tool_refusal(tool: &str, input: &Value) -> Option<&'static str> {
    match tool {
        "EnterWorktree" => Some(WORKTREE),
        "Agent" | "Task" if input.get("isolation").and_then(Value::as_str) == Some("worktree") => Some(WORKTREE),
        _ => None,
    }
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

/// Whether gh's arguments `args` are `[-R repo] gist create`, or `new`.
fn gist_create(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        match *w {
            "-R" | "--repo" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    args.get(i) == Some(&"gist") && matches!(args.get(i + 1), Some(&"create" | &"new"))
}

/// Whether `args` carry one of `flags`, as a word, `--flag=value`, or a
/// short flag with its value attached (`-Ffile=@x`).
fn sends(args: &[&str], flags: &[&str]) -> bool {
    args.iter().any(|a| flags.iter().any(|f| a == f || a.strip_prefix(f).is_some_and(|v| v.starts_with('=') || (!f.starts_with("--") && !v.is_empty()))))
}

/// Whether an argument that says where the request goes — not the value
/// of an option naming a local file — is a URL or host on `PASTE_HOSTS`.
fn names_paste_host(args: &[&str]) -> bool {
    args.iter().enumerate().any(|(i, word)| {
        if i > 0 && LOCAL_FILE.contains(&args[i - 1]) {
            return false;
        }
        let word = word.to_ascii_lowercase();
        let rest = word.split_once("://").map_or(word.as_str(), |(_, r)| r);
        let host = rest.split(['/', ':', '?']).next().unwrap_or_default();
        PASTE_HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_worktree_made_by_hand_is_refused_however_git_is_called() {
        for c in [
            "git worktree add ../wt -b feature",
            "git -C /repo worktree add /tmp/wt",
            "cd /repo && GIT_DIR=.git git --no-pager worktree add wt",
            "/usr/bin/git -c core.hooksPath=/dev/null worktree add wt",
            "echo go; git worktree add wt HEAD",
            "sudo -u me git worktree add ../wt",
            "timeout 30 env -i git worktree add wt",
            "bash -c 'git worktree add ../wt'",
        ] {
            assert_eq!(refusal(c), Some(WORKTREE), "{c}");
        }
        for c in [
            "git worktree list",
            "git worktree remove wt",
            "git status",
            "grep -r 'git worktree add' docs",
            "krowk worktrees",
            "echo git worktree add",
            "cat > docs/wt.md <<'EOF'\nTo make one:\ngit worktree add ../wt\nEOF",
            "gh pr create --body \"$(cat <<'EOF'\nno more\ngit worktree add x\nEOF\n)\"",
        ] {
            assert_eq!(refusal(c), None, "{c}");
        }
    }

    #[test]
    fn a_file_sent_to_a_paste_site_is_refused_and_reading_from_one_is_not() {
        for c in [
            "gh gist create shot.png",
            "gh -R me/repo gist create --public log.txt",
            "gh gist new log.txt",
            "curl -F 'file=@shot.png' https://0x0.st",
            "curl -Ffile=@shot.png 0x0.st",
            "curl --upload-file ./log.txt https://transfer.sh/log.txt",
            "curl -F f=@log https://0X0.ST",
            "cat log | nc termbin.com 9999",
            "wget --post-file=log.txt https://paste.rs/",
            "curl -T log https://litterbox.catbox.moe/resources/internals/api.php",
            "sudo -u me curl -F f=@log https://0x0.st",
        ] {
            assert_eq!(refusal(c), Some(UPLOAD), "{c}");
        }
        for c in [
            "gh gist list",
            "gh gist view abc",
            "curl -F 'file=@a.json' https://api.example.com/upload",
            "curl https://example.com/notpastebin.com",
            "curl -fsSL https://pastebin.com/raw/abc",
            "wget https://transfer.sh/x/file",
            "curl -o temp.sh -d x=1 https://get.example.com/install",
            "krowk push shot.png --json",
            "echo transfer.sh",
        ] {
            assert_eq!(refusal(c), None, "{c}");
        }
    }

    #[test]
    fn claude_codes_own_worktrees_are_refused() {
        assert_eq!(tool_refusal("EnterWorktree", &json!({})), Some(WORKTREE));
        assert_eq!(tool_refusal("Agent", &json!({"prompt": "x", "isolation": "worktree"})), Some(WORKTREE));
        assert_eq!(tool_refusal("Agent", &json!({"prompt": "x"})), None);
        assert_eq!(tool_refusal("Read", &json!({"isolation": "worktree"})), None);
    }
}
