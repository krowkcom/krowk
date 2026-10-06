//! The paste guard: a `gh pr|issue create|edit|comment` whose body carries
//! a bare krowk card link is refused, that one call, with a message telling
//! the model to paste the krowk block instead. GitHub does not unfurl a
//! card link; bare, it is a blue link that says nothing about the file.
//!
//! It guards the shape of a paste, not what may run, so it is no permission
//! rule: it holds in every mode, `bypassPermissions` and `unhinged`
//! included. The native loop asks it before a bash call's hooks; Claude
//! Code asks it through a `PreToolUse` hook callback krowk registers on
//! `initialize`; Codex runs it as a `PreToolUse` command hook,
//! `krowk __paste-guard` (`HOOK_ARG`), answered by `pre_tool_use`.
//!
//! A link is a card link when its path is `/a/art_<slug>` — the card page,
//! or its `share_url` with `?share=`. It is fine as the target of a krowk
//! block line (canon glossary → Paste form): `[![caption](file_url)](url)`
//! or `… · [View preview ↗](url)`. Anywhere else in the body it is bare.
//!
//! The body is what `--body`/`-b` gives, a heredoc feeding it, or the file
//! `--body-file`/`-F` names when it can be read. Bodies posted through
//! `gh api`, an MCP server or a browser are not seen.

use crate::permissions::rules;
use serde_json::{json, Value};
use std::path::Path;

/// The argument Codex's hook runs krowk with: `krowk __paste-guard`, the
/// hook's input on stdin, its answer on stdout.
pub const HOOK_ARG: &str = "__paste-guard";

/// The most of a `--body-file` read: a body is a comment, not a log.
const MAX_BODY_FILE: u64 = 1 << 20;

const ARTIFACT_PATH: &str = "/a/art_";
const SLUG_LENGTH: usize = 24;

/// Why `command` is not run, when it posts a bare card link to GitHub.
/// `cwd` is where a `--body-file` is read from.
pub fn refusal(command: &str, cwd: &Path) -> Option<String> {
    if !command.contains("gh") {
        return None;
    }
    let (rest, heredocs) = heredocs(command);
    let bodies: Vec<String> = rules::split(&rest).commands.iter().filter_map(gh_post).flat_map(|args| bodies(&args, &heredocs, cwd)).collect();
    let bare = bodies.iter().find_map(|b| bare_link(b))?;
    Some(message(&bare))
}

/// The refusal the model reads.
fn message(link: &str) -> String {
    format!(
        "krowk did not run this: the body carries a bare krowk card link ({link}), and GitHub shows a bare card link as a plain link that tells the reader nothing. \
         Put the artifact's krowk block there instead — `paste.markdown` from `krowk uploads show <link> --json`, the `markdown` form on a publish result, or what `krowk push FILE --destination github` prints — and run the command again with it. \
         A card link inside a krowk block is fine."
    )
}

/// A `PreToolUse` hook's answer, Claude Code's shape, which Codex also
/// reads: a deny with the refusal, or nothing to say.
pub fn pre_tool_use(input: &Value) -> Value {
    let command = (input.get("tool_name").and_then(Value::as_str) == Some("Bash")).then(|| input.pointer("/tool_input/command").and_then(Value::as_str)).flatten();
    let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or(".");
    match command.and_then(|c| refusal(c, Path::new(cwd))) {
        Some(why) => json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why}}),
        None => json!({}),
    }
}

/// `pre_tool_use` over the hook's stdin, for `krowk __paste-guard`: an
/// input it cannot read is let through, as a hook with nothing to say.
pub fn hook_main(stdin: &str) -> String {
    serde_json::from_str::<Value>(stdin).map(|v| pre_tool_use(&v)).unwrap_or_else(|_| json!({})).to_string()
}

/// The arguments after `gh pr|issue create|edit|comment`, each with
/// whether the shell computes it, when `c` is such a call.
fn gh_post(c: &rules::Simple) -> Option<Vec<(String, bool)>> {
    let words = &c.words;
    let at = words.iter().position(|w| !is_assignment(w) && !matches!(w.as_str(), "env" | "command" | "exec" | "nohup" | "time"))?;
    let program = words[at].rsplit('/').next().unwrap_or_default();
    let (noun, verb) = (words.get(at + 1)?, words.get(at + 2)?);
    let posts = program == "gh" && matches!(noun.as_str(), "pr" | "issue") && matches!(verb.as_str(), "create" | "edit" | "comment");
    posts.then(|| words.iter().zip(&c.dynamic).skip(at + 3).map(|(w, d)| (w.clone(), *d)).collect())
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()))
}

/// The texts a gh call posts: each `--body`, and each `--body-file` that
/// can be read. A body the shell computes, or one read from stdin, is
/// judged by the heredocs on the line, which is where an agent writes it.
fn bodies(args: &[(String, bool)], heredocs: &[String], cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let word = args[i].0.as_str();
        let (flag, value, at) = match word.split_once('=') {
            Some((f @ ("--body" | "--body-file"), v)) => (f, Some(v.to_string()), i),
            _ => match word {
                "--body" | "-b" | "--body-file" | "-F" => (word, args.get(i + 1).map(|a| a.0.clone()), i + 1),
                w if w.len() > 2 && (w.starts_with("-b") || w.starts_with("-F")) && !w.starts_with("--") => (&w[..2], Some(w[2..].to_string()), i),
                _ => ("", None, i),
            },
        };
        i = at + 1;
        let Some(value) = value else { continue };
        let computed = args.get(at).is_some_and(|a| a.1);
        match flag {
            "--body" | "-b" => {
                if computed {
                    out.extend(heredocs.iter().cloned());
                }
                out.push(value);
            }
            "--body-file" | "-F" if value == "-" => out.extend(heredocs.iter().cloned()),
            "--body-file" | "-F" => out.extend(read_body(&cwd.join(&value))),
            _ => {}
        }
    }
    out
}

fn read_body(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BODY_FILE {
        return None;
    }
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The command with every heredoc's body taken out, and the bodies.
/// `<<`, `<<-`, and the delimiter quoted or not; `<<<` is a here-string
/// and has no body.
fn heredocs(command: &str) -> (String, Vec<String>) {
    let (mut rest, mut bodies) = (String::new(), Vec::new());
    let mut lines = command.split('\n');
    while let Some(line) = lines.next() {
        rest.push_str(line);
        rest.push('\n');
        for (delimiter, tabs) in markers(line) {
            let mut body = String::new();
            for l in lines.by_ref() {
                if (if tabs { l.trim_start_matches('\t') } else { l }) == delimiter {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            bodies.push(body);
        }
    }
    rest.pop();
    (rest, bodies)
}

/// The heredoc delimiters a line opens, in order, and whether each is
/// `<<-` (its closing line may be indented with tabs).
fn markers(line: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut s = line;
    while let Some(at) = s.find("<<") {
        let mut after = &s[at + 2..];
        if after.starts_with('<') {
            s = after.trim_start_matches('<');
            continue;
        }
        let tabs = after.starts_with('-');
        after = after.trim_start_matches('-').trim_start();
        let end = after.find(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | ')' | '<' | '>')).unwrap_or(after.len());
        let delimiter: String = after[..end].chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
        if !delimiter.is_empty() {
            out.push((delimiter, tabs));
        }
        s = &after[end..];
    }
    out
}

/// The first card link in `text` that is not the target of a krowk block
/// line.
fn bare_link(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut from = 0;
        while let Some(found) = line[from..].find(ARTIFACT_PATH) {
            let at = from + found;
            from = at + ARTIFACT_PATH.len();
            let Some((start, url)) = card_link(line, at) else { continue };
            if !in_block(&line[..start]) {
                return Some(url);
            }
        }
        None
    })
}

/// The card link whose `/a/art_` is at `at`: where it starts, and the
/// link. None when the slug is not one, or no host comes before it.
fn card_link(line: &str, at: usize) -> Option<(usize, String)> {
    let stop = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'' | '`');
    let start = line[..at].rfind(stop).map_or(0, |i| i + line[i..].chars().next().map_or(1, char::len_utf8));
    let slug: String = line[at + ARTIFACT_PATH.len()..].chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect();
    let host = &line[start..at];
    if slug.len() < SLUG_LENGTH || !(host.contains("://") || host.trim_start_matches("www.").starts_with("krowk.com")) {
        return None;
    }
    let end = line[at..].find(stop).map_or(line.len(), |i| at + i);
    Some((start, line[start..end].to_string()))
}

/// Whether what precedes a link makes it a krowk block line's target:
/// `[View preview ↗](` on the caption line, `[![caption](file_url)](` on
/// an image's.
fn in_block(before: &str) -> bool {
    before.ends_with("[View preview ↗](") || (before.ends_with(")](") && before.contains("[!["))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "https://krowk.com/a/art_0123456789abcdefghijklmn";
    const CARD2: &str = "https://krowk.com/a/art_nmlkjihgfedcba9876543210";
    const FILE: &str = "https://cdn.krowk.com/f/0123456789abcdefghijklmn/shot.png";

    fn refused(cmd: &str) -> bool {
        refusal(cmd, Path::new("/nonexistent")).is_some()
    }

    fn block(caption: &str, url: &str) -> String {
        format!("**{caption}** · [View preview ↗]({url})")
    }

    fn image_block(caption: &str, url: &str) -> String {
        format!("[![{caption}]({FILE})]({url})\n{caption} · [View preview ↗]({url})")
    }

    #[test]
    fn every_gh_post_with_a_bare_link_inline_is_refused() {
        for noun in ["pr", "issue"] {
            for verb in ["create", "edit", "comment"] {
                for flag in ["--body", "-b"] {
                    let cmd = format!("gh {noun} {verb} 12 {flag} 'The fix: {CARD}'");
                    assert!(refused(&cmd), "{cmd}");
                }
                let cmd = format!("gh {noun} {verb} 12 --body='See {CARD}'");
                assert!(refused(&cmd), "{cmd}");
            }
        }
    }

    #[test]
    fn the_block_the_registry_hands_back_goes_through() {
        let block = "[![cyton-eog.png](https://cdn.krowkusercontent.com/weur/ws_xuv48ya3we0s0mf0bftjsu20/art_5jed46cyrzwfamf3aegkf4lw/cyton-eog.png)](https://krowk.com/a/art_5jed46cyrzwfamf3aegkf4lw)\ncyton-eog.png · [View preview ↗](https://krowk.com/a/art_5jed46cyrzwfamf3aegkf4lw)";
        assert!(!refused(&format!("gh pr comment 12 --body 'The EOG trace:\n\n{block}'")));
    }

    #[test]
    fn the_krowk_block_goes_through() {
        let body = format!("Before and after:\n\n{}\n\n{}", image_block("Cart before", CARD), block("Build log", CARD2));
        assert!(!refused(&format!("gh pr comment 12 --body '{body}'")));
        assert!(!refused(&format!("gh issue create -t Bug -b \"{}\"", block("Trace", CARD))));
    }

    #[test]
    fn a_multi_artifact_block_goes_through_and_a_stray_link_beside_it_does_not() {
        let run = "Part of [run](https://krowk.com/r/run_0123456789abcdefghijklmn) · 3 artifacts";
        let body = format!("{}\n{}\n{}\n{run}", image_block("One", CARD), block("Two", CARD2), block("Three", CARD));
        assert!(!refused(&format!("gh pr comment 1 --body '{body}'")));
        assert!(refused(&format!("gh pr comment 1 --body '{body}\nalso {CARD2}'")));
    }

    #[test]
    fn a_link_under_another_label_or_share_url_is_bare() {
        assert!(refused(&format!("gh pr comment 1 --body '[screenshot]({CARD})'")));
        assert!(refused(&format!("gh pr comment 1 --body '<{CARD}>'")));
        assert!(refused(&format!("gh pr comment 1 --body 'see {CARD}?share=tok_abc'")));
        assert!(refused("gh pr comment 1 --body 'see krowk.com/a/art_0123456789abcdefghijklmn'"));
        assert!(!refused(&format!("gh pr comment 1 --body '{}'", block("Shared", &format!("{CARD}?share=tok_abc")))));
    }

    #[test]
    fn a_heredoc_body_is_read() {
        let bare = format!("gh pr comment 7 --body \"$(cat <<'EOF'\nHere it is: {CARD}\nEOF\n)\"");
        assert!(refused(&bare));
        let ok = format!("gh pr create --title T --body \"$(cat <<'EOF'\n## Summary\n\n{}\nEOF\n)\"", block("Diff", CARD));
        assert!(!refused(&ok));
        let stdin = format!("gh issue comment 3 --body-file - <<-EOF\n\tsee {CARD}\n\tEOF");
        assert!(refused(&stdin));
    }

    #[test]
    fn a_body_file_is_read_when_it_can_be() {
        let dir = std::env::temp_dir().join(format!("krowk-paste-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bare.md"), format!("Result: {CARD}\n")).unwrap();
        std::fs::write(dir.join("block.md"), format!("Result:\n{}\n", block("Result", CARD))).unwrap();
        for flag in ["--body-file ", "-F ", "--body-file=", "-F"] {
            assert!(refusal(&format!("gh pr edit 4 {flag}bare.md"), &dir).is_some(), "{flag}");
            assert!(refusal(&format!("gh pr edit 4 {flag}block.md"), &dir).is_none(), "{flag}");
        }
        assert!(refusal("gh pr comment 4 -F missing.md", &dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn other_commands_and_link_free_bodies_are_never_touched() {
        assert!(!refused(&format!("gh pr view 12 --comments # {CARD}")));
        assert!(!refused(&format!("gh api repos/o/r/issues/1/comments -f body='{CARD}'")));
        assert!(!refused("gh pr comment 12 --body 'LGTM, merging'"));
        assert!(!refused("gh pr edit 12 --add-label bug"));
        assert!(!refused(&format!("echo {CARD} && gh pr comment 1 --body ok")));
        assert!(!refused(&format!("basecamp comments create 123 '{CARD}'")));
        assert!(!refused(&format!("curl -X POST https://slack.com/api/chat.postMessage -d 'text={CARD}'")));
    }

    #[test]
    fn found_through_a_chain_an_assignment_or_a_path() {
        assert!(refused(&format!("cd repo && GH_REPO=o/r /usr/bin/gh pr comment 1 -b 'x {CARD}'")));
        assert!(refused(&format!("git push && env GH_TOKEN=t gh issue create -t T -b '{CARD}'")));
    }

    #[test]
    fn the_hook_answer_denies_with_the_refusal() {
        let deny = pre_tool_use(&json!({"tool_name": "Bash", "tool_input": {"command": format!("gh pr comment 1 --body '{CARD}'")}, "cwd": "/"}));
        assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(deny["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("krowk block"));
        assert_eq!(pre_tool_use(&json!({"tool_name": "Bash", "tool_input": {"command": "gh pr comment 1 --body ok"}})), json!({}));
        assert_eq!(pre_tool_use(&json!({"tool_name": "Read", "tool_input": {"file_path": CARD}})), json!({}));
        assert_eq!(hook_main("not json"), "{}");
    }
}
