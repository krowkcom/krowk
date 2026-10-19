//! The tooling guard: an agent that reaches past krowk for what krowk
//! does itself is refused, that one call, with a message naming krowk's way.
//!
//! - **A worktree by hand**: `git worktree add`, Claude Code's
//!   `EnterWorktree`, or its `Agent` called with `isolation: "worktree"`. A
//!   worktree krowk makes is locked to its session, readied (submodules,
//!   seeds, `worktrees.setup`), held against the machine's agent slots and
//!   finished when the work ends; one made any other way is none of that,
//!   and `krowk worktrees` never sees it. (An agent definition's own
//!   `isolation` frontmatter is Claude Code's to read, not the call's, and
//!   is not seen.)
//! - **A file sent to a paste site**: a gist made or edited (`gh gist
//!   create|new|edit`, a write to `gh api gists`), an upload by `curl` or
//!   `wget` to one of `PASTE_HOSTS`, or `nc` to one. krowk publishes a file
//!   as an artifact, whose link unfurls and stays in the person's
//!   workspace. Reading from one is fine.
//!
//! It runs with the paste guard (`crate::paste_guard::guard`), on every
//! backend the same way, and like it holds in every mode: it steers the
//! agent, not the person, who can run any of these at their own prompt.
//! A command is judged in every form a deny rule is (`rules::forms`):
//! behind `sudo`, `env`, `timeout` and the like, and inside `bash -c`. A
//! heredoc's body is text, unless a shell reads it. One the shell computes,
//! or one a script runs, is not seen.

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

/// Programs that run the heredoc they read.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash", "fish"];

/// What a worktree made by hand is refused with.
pub const WORKTREE: &str = "krowk did not run this: krowk makes the worktrees agents work in, so each is locked to its session, readied and finished, and `krowk worktrees` lists it. \
     To work apart from the checkout, the person starts krowk with `--worktree` or sets `worktrees.default`; in krowk's own agent, a `subagent` call with `isolation: \"worktree\"` gives a child one. \
     Otherwise work where you are. A person can still make one by hand at their own prompt.";

/// What a paste-site upload is refused with.
pub const UPLOAD: &str = "krowk did not run this: files are published as krowk artifacts, not to a paste site. \
     Use the `publish` tool, or `krowk push FILE --json`, and paste the link or the `markdown` form it returns.";

/// How a program's options read: the long ones and short letters that
/// send a file or data, whether a writing `--method` does, and the long
/// ones and short letters that take a value.
struct Options {
    sends: &'static [&'static str],
    short_sends: &'static str,
    method: bool,
    valued: &'static [&'static str],
    valued_short: &'static str,
}

const CURL: Options = Options {
    sends: &["--form", "--form-string", "--upload-file", "--data", "--data-ascii", "--data-binary", "--data-raw", "--data-urlencode", "--json"],
    short_sends: "FTd",
    method: false,
    valued: &[
        "--form", "--form-string", "--upload-file", "--data", "--data-ascii", "--data-binary", "--data-raw", "--data-urlencode", "--json", "--header", "--output", "--user-agent", "--user", "--proxy", "--referer", "--cookie",
        "--cookie-jar", "--dump-header", "--config", "--request", "--write-out", "--max-time", "--connect-timeout", "--range", "--cert", "--key", "--cacert", "--url",
    ],
    valued_short: "AbcCdDeEFHKmoPQrTtuUwxXyYz",
};

const WGET: Options = Options {
    sends: &["--post-file", "--post-data", "--body-file", "--body-data"],
    short_sends: "",
    method: true,
    valued: &[
        "--post-file", "--post-data", "--body-file", "--body-data", "--method", "--header", "--output-document", "--output-file", "--append-output", "--user-agent", "--user", "--password", "--directory-prefix",
        "--execute", "--input-file", "--tries", "--timeout", "--base",
    ],
    valued_short: "aBDeilOoPQTtUwX",
};

const NC: Options = Options { sends: &[], short_sends: "", method: false, valued: &[], valued_short: "cepqsiwxX" };

/// Why `command` is not run, when it makes a worktree by hand or sends a
/// file to a paste site.
pub fn refusal(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    if !(lower.contains("worktree") || lower.contains("gist") || PASTE_HOSTS.iter().any(|h| lower.contains(h))) {
        return None;
    }
    let (rest, heredocs) = crate::paste_guard::heredocs(command);
    judged(&rest).or_else(|| heredocs.iter().filter(|h| read_by_shell(&h.opener)).find_map(|h| refusal(&h.body)))
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

/// `refusal` of a command line with its heredoc bodies taken out.
fn judged(line: &str) -> Option<&'static str> {
    rules::split(line).commands.iter().flat_map(rules::forms).find_map(|form| {
        let (program, args) = form.split_first()?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match basename(program) {
            "git" => worktree_add(&args).then_some(WORKTREE),
            "gh" => gist_written(&args).then_some(UPLOAD),
            "curl" => (sends(&args, &CURL) && to_paste_host(&args, &CURL)).then_some(UPLOAD),
            "wget" => (sends(&args, &WGET) && to_paste_host(&args, &WGET)).then_some(UPLOAD),
            // `nc -z` only asks whether the port is open.
            "nc" | "ncat" | "netcat" => (!args.iter().any(|a| short_group(a).is_some_and(|g| g.contains('z'))) && to_paste_host(&args, &NC)).then_some(UPLOAD),
            _ => None,
        }
    })
}

/// Whether the command a heredoc was opened in is a shell, which runs it.
fn read_by_shell(opener: &str) -> bool {
    rules::split(opener).commands.iter().flat_map(rules::forms).any(|f| f.first().is_some_and(|p| SHELLS.contains(&basename(p))))
}

fn basename(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
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

/// Whether gh's arguments `args` make or change a gist: `gist create`,
/// `new` or `edit`, or `api gists…` with fields or a writing method.
fn gist_written(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        match *w {
            "-R" | "--repo" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    match (args.get(i), args.get(i + 1)) {
        (Some(&"gist"), Some(&("create" | "new" | "edit"))) => true,
        (Some(&"api"), _) => {
            let rest = &args[i + 1..];
            let gists = rest.iter().any(|a| a.trim_start_matches('/') == "gists" || a.trim_start_matches('/').starts_with("gists/"));
            let fields = rest.iter().any(|a| ["-f", "-F", "--field", "--raw-field", "--input"].iter().any(|f| a == f || a.starts_with(&format!("{f}="))));
            let method = rest.windows(2).any(|w| matches!(w[0], "-X" | "--method") && !w[1].eq_ignore_ascii_case("GET")) || rest.iter().any(|a| a.strip_prefix("--method=").is_some_and(|m| !m.eq_ignore_ascii_case("GET")));
            gists && (fields || method)
        }
        _ => false,
    }
}

/// The letters of a short option group, `-sF` → `sF`; none for a long
/// option, a lone `-`, or a word that is not an option.
fn short_group(word: &str) -> Option<&str> {
    word.strip_prefix('-').filter(|g| !g.is_empty() && !g.starts_with('-'))
}

/// Whether `args` send a file or data: one of `o.sends`, as a word or
/// with `=value`; a letter of `o.short_sends` in a short group, before any
/// letter that takes the rest as its value; or, where `o.method`, a
/// `--method` other than GET or HEAD.
fn sends(args: &[&str], o: &Options) -> bool {
    args.iter().enumerate().any(|(i, a)| {
        let method = match a.strip_prefix("--method") {
            Some("") => args.get(i + 1).copied(),
            Some(v) => v.strip_prefix('='),
            None => None,
        };
        o.sends.iter().any(|f| a == f || a.strip_prefix(f).is_some_and(|v| v.starts_with('=')))
            || (o.method && method.is_some_and(|m| !m.eq_ignore_ascii_case("GET") && !m.eq_ignore_ascii_case("HEAD")))
            || short_group(a).and_then(|g| g.chars().find(|c| o.short_sends.contains(*c) || o.valued_short.contains(*c))).is_some_and(|c| o.short_sends.contains(c))
    })
}

/// Whether an operand that says where the request goes — not an option's
/// value, which may be a local file or a payload — is a URL or host on
/// `PASTE_HOSTS`. `--url`'s value is one.
fn to_paste_host(args: &[&str], o: &Options) -> bool {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        if let Some(url) = a.strip_prefix("--url=") {
            if on_paste_host(url) {
                return true;
            }
        } else if *a == "--url" {
            if args.get(i).is_some_and(|u| on_paste_host(u)) {
                return true;
            }
            i += 1;
        } else if a.starts_with("--") {
            if !a.contains('=') && o.valued.contains(a) {
                i += 1;
            }
        } else if let Some(g) = short_group(a) {
            // The first letter taking a value takes the rest of the group,
            // or else the next word.
            if g.char_indices().find(|(_, c)| o.valued_short.contains(*c)).is_some_and(|(at, _)| at + 1 == g.len()) {
                i += 1;
            }
        } else if on_paste_host(a) {
            return true;
        }
    }
    false
}

/// Whether `word` is a URL or host on one of `PASTE_HOSTS`.
fn on_paste_host(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    let rest = word.split_once("://").map_or(word.as_str(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default().split(':').next().unwrap_or_default();
    PASTE_HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
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
            "bash <<'EOF'\ncd /repo\ngit worktree add ../wt\nEOF",
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
            "gh gist edit abc123 -a secret.log",
            "gh api gists -f 'files[log.txt][content]=x'",
            "gh api -X POST /gists --input body.json",
            "curl -F 'file=@shot.png' https://0x0.st",
            "curl -Ffile=@shot.png 0x0.st",
            "curl -sF 'file=@shot.png' https://0x0.st",
            "curl -sST log https://transfer.sh/log",
            "curl --upload-file ./log.txt https://transfer.sh/log.txt",
            "curl -F f=@log https://0X0.ST",
            "curl -F f=@log https://x@0x0.st",
            "curl -F f=@log --url https://0x0.st",
            "cat log | nc termbin.com 9999",
            "wget --post-file=log.txt https://paste.rs/",
            "wget --method=PUT --body-file=log https://transfer.sh/log",
            "curl -T log https://litterbox.catbox.moe/resources/internals/api.php",
            "sudo -u me curl -F f=@log https://0x0.st",
            "sh <<EOF\ncurl -F f=@log https://0x0.st\nEOF",
        ] {
            assert_eq!(refusal(c), Some(UPLOAD), "{c}");
        }
        for c in [
            "gh gist list",
            "gh gist view abc",
            "gh api gists",
            "curl -F 'file=@a.json' https://api.example.com/upload",
            "curl https://example.com/notpastebin.com",
            "curl -fsSL https://pastebin.com/raw/abc",
            "wget https://transfer.sh/x/file",
            "wget --method=GET https://transfer.sh/abc/file.txt",
            "nc -z termbin.com 9999",
            "curl -o temp.sh -d x=1 https://get.example.com/install",
            "curl -X POST -H 'Content-Type: application/json' -d '{\"text\":\"log at https://pastebin.com/raw/x\"}' https://hooks.slack.com/x",
            "curl -sodata.json -d x=1 https://api.example.com",
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
