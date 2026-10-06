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
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// The argument Codex's hook runs krowk with: `krowk __paste-guard`, the
/// hook's input on stdin, its answer on stdout.
pub const HOOK_ARG: &str = "__paste-guard";

/// The most of a `--body-file` read: a body is a comment, not a log.
const MAX_BODY_FILE: u64 = 1 << 20;

const ARTIFACT_PATH: &str = "/a/art_";
const SLUG_LENGTH: usize = 24;

/// Why `command` is not run, when it posts a bare card link to GitHub.
/// `cwd` is where a `--body-file` is read from, moved by a `cd` before it.
pub fn refusal(command: &str, cwd: &Path) -> Option<String> {
    if !command.contains("gh") {
        return None;
    }
    let (rest, heredocs) = heredocs(command);
    let mut dir = cwd.to_path_buf();
    let mut bodies = Vec::new();
    for c in rules::split(&rest).commands {
        match c.words.as_slice() {
            [cd, to] if cd == "cd" && !c.dynamic[1] => dir = dir.join(to),
            _ => bodies.extend(gh_post(&c).map(|args| bodies_of(&args, &heredocs, &dir)).unwrap_or_default()),
        }
    }
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

/// The arguments after `gh [--repo R] pr|issue create|edit|comment`, each
/// with whether the shell computes it, when `c` is such a call.
fn gh_post(c: &rules::Simple) -> Option<Vec<(String, bool)>> {
    let words: Vec<&str> = c.words.iter().map(String::as_str).collect();
    let at = words.iter().position(|w| !is_assignment(w) && !matches!(*w, "env" | "command" | "exec" | "nohup" | "time"))?;
    let args = post_args(&words, at)?;
    Some(c.words.iter().cloned().zip(c.dynamic.iter().copied()).skip(args).collect())
}

/// Where the arguments start when `words[at]` begins a gh post.
fn post_args(words: &[&str], at: usize) -> Option<usize> {
    if words[at].rsplit('/').next() != Some("gh") {
        return None;
    }
    let mut noun = at + 1;
    while let Some(w) = words.get(noun) {
        match *w {
            "-R" | "--repo" => noun += 2,
            w if w.starts_with("--repo=") => noun += 1,
            _ => break,
        }
    }
    let posts = matches!(*words.get(noun)?, "pr" | "issue") && matches!(*words.get(noun + 1)?, "create" | "edit" | "comment");
    posts.then_some(noun + 2)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()))
}

/// The texts a gh call posts. A `--body` as given; one the shell computes
/// is judged by the heredocs opened on the gh call's lines, which is where
/// an agent writes it, and so is a `--body-file` read from stdin. Any
/// other `--body-file` is the heredoc the command writes it from, else the
/// file, when it can be read.
fn bodies_of(args: &[(String, bool)], heredocs: &[Heredoc], cwd: &Path) -> Vec<String> {
    let feeding = || heredocs.iter().filter(|h| h.feeds_gh_post()).map(|h| h.body.clone());
    let mut out = Vec::new();
    for (flag, value, computed) in body_flags(args) {
        match flag {
            "--body" if computed => out.extend(feeding().chain([value])),
            "--body" => out.push(value),
            _ if matches!(value.as_str(), "-" | "/dev/stdin") => out.extend(feeding()),
            _ => {
                let written: Vec<String> = heredocs.iter().filter(|h| h.writes(&value, cwd)).map(|h| h.body.clone()).collect();
                if written.is_empty() {
                    out.extend(read_body(&cwd.join(&value)));
                }
                out.extend(written);
            }
        }
    }
    out
}

/// Each body flag a gh call has, as `--body` or `--body-file`, with its
/// value and whether the shell computes that.
fn body_flags(args: &[(String, bool)]) -> Vec<(&'static str, String, bool)> {
    let canonical = |f: &str| if matches!(f, "--body" | "-b") { "--body" } else { "--body-file" };
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let (word, computed) = (args[i].0.as_str(), args[i].1);
        match word.split_once('=') {
            Some((f @ ("--body" | "--body-file"), v)) => out.push((canonical(f), v.to_string(), computed)),
            _ => match word {
                "--body" | "-b" | "--body-file" | "-F" => {
                    if let Some((v, d)) = args.get(i + 1) {
                        out.push((canonical(word), v.clone(), *d));
                    }
                    i += 1;
                }
                w if w.len() > 2 && (w.starts_with("-b") || w.starts_with("-F")) && !w.starts_with("--") => out.push((canonical(&w[..2]), w[2..].to_string(), computed)),
                _ => {}
            },
        }
        i += 1;
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

/// One heredoc: the command line it was opened on, continuation lines
/// joined, up to the line that opened it; and its body.
struct Heredoc {
    opener: String,
    body: String,
}

impl Heredoc {
    /// Opened on a gh post's own command line: the `$(cat <<'EOF'` of a
    /// `--body`, or the `<<EOF` of a `--body-file -`.
    fn feeds_gh_post(&self) -> bool {
        let words: Vec<&str> = self.opener.split_whitespace().collect();
        (0..words.len()).any(|at| post_args(&words, at).is_some())
    }

    /// Opened by a redirection into `path`, or a `tee` of it, however
    /// either is spelled: quoted, or by `./`.
    fn writes(&self, path: &str, cwd: &Path) -> bool {
        let wanted = normal(&cwd.join(path));
        let names = |rest: &str| {
            let target = rest.trim_start().split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>')).next().unwrap_or_default();
            let target = target.trim_matches(|c| c == '\'' || c == '"');
            !target.is_empty() && normal(&cwd.join(target)) == wanted
        };
        self.opener.split('>').skip(1).any(names) || self.opener.split("tee ").skip(1).any(names)
    }
}

/// A path without its `.` components.
fn normal(path: &Path) -> PathBuf {
    path.components().filter(|c| *c != Component::CurDir).collect()
}

/// The command with every heredoc's body taken out, and the heredocs.
/// `<<`, `<<-`, and the delimiter quoted or not; `<<<` is a here-string
/// and has no body. A `<<` whose delimiter never closes a line is text — a
/// shift in a quoted body — and is left where it is.
fn heredocs(command: &str) -> (String, Vec<Heredoc>) {
    let lines: Vec<&str> = command.split('\n').collect();
    // Where each line stands, as it is and with `<<-`'s leading tabs off,
    // so finding a delimiter's line is a lookup, not a scan.
    let mut exact: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut tabbed: HashMap<&str, Vec<usize>> = HashMap::new();
    for (n, l) in lines.iter().enumerate() {
        exact.entry(l).or_default().push(n);
        tabbed.entry(l.trim_start_matches('\t')).or_default().push(n);
    }
    let (mut rest, mut docs, mut logical) = (Vec::new(), Vec::new(), String::new());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !rest.last().is_some_and(|l: &&str| l.ends_with('\\')) {
            logical.clear();
        }
        logical.push_str(line);
        logical.push(' ');
        rest.push(line);
        i += 1;
        for (delimiter, tabs) in markers(line) {
            let at = (if tabs { &tabbed } else { &exact }).get(delimiter.as_str()).and_then(|ns| ns.get(ns.partition_point(|&n| n < i)));
            let Some(&close) = at else { continue };
            docs.push(Heredoc { opener: logical.clone(), body: lines[i..close].join("\n") });
            i = close + 1;
        }
    }
    (rest.join("\n"), docs)
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
/// line. Code — a fenced block, or a backtick span — quotes a link rather
/// than pastes it, and is passed over.
fn bare_link(text: &str) -> Option<String> {
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced
            && line.contains(ARTIFACT_PATH)
            && let Some(link) = bare_in(line)
        {
            return Some(link);
        }
    }
    None
}

/// A line's first bare card link, in one pass over its words.
fn bare_in(line: &str) -> Option<String> {
    let stop = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'' | '`');
    let image = line.find("[![");
    let (mut start, mut ticks) = (0, 0);
    for (i, c) in line.char_indices().filter(|&(_, c)| stop(c)).chain([(line.len(), ' ')]) {
        if ticks % 2 == 0
            && let Some(link) = card_link(&line[start..i])
            && !in_block(&line[..start], image)
        {
            return Some(link);
        }
        ticks += usize::from(c == '`');
        start = i + c.len_utf8();
    }
    None
}

/// The card link a word is, trimmed of the punctuation around it: a link
/// whose path is `/a/art_<slug>`, with a host before it.
fn card_link(word: &str) -> Option<String> {
    let word = word.trim_matches(|c: char| matches!(c, '*' | '_' | '.' | ',' | ';' | ':' | '!' | '?'));
    let at = word.find(ARTIFACT_PATH)?;
    let slug = word[at + ARTIFACT_PATH.len()..].chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).count();
    let host = &word[..at];
    (slug >= SLUG_LENGTH && (host.contains("://") || host.trim_start_matches("www.").starts_with("krowk.com"))).then(|| word.to_string())
}

/// Whether what precedes a link makes it a krowk block line's target:
/// `[View preview ↗](` on the caption line, `[![caption](file_url)](` on
/// an image's (`image`, where the line's first `[![` is).
fn in_block(before: &str, image: Option<usize>) -> bool {
    before.ends_with("[View preview ↗](") || (before.ends_with(")](") && image.is_some_and(|at| at < before.len()))
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
    fn a_body_file_written_on_the_same_line_is_judged_by_what_is_written() {
        let dir = std::env::temp_dir().join(format!("krowk-paste-guard-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // What an earlier, refused attempt left behind.
        std::fs::write(dir.join("body.md"), format!("see {CARD}\n")).unwrap();
        let rewrite = |body: &str| format!("cat > body.md <<'EOF'\n{body}\nEOF\ngh pr comment 1 --body-file body.md");
        assert!(refusal(&rewrite(&block("Cart", CARD)), &dir).is_none(), "the rewrite is what is posted");
        assert!(refusal(&rewrite(&format!("see {CARD}")), &dir.join("sub")).is_some(), "and a bare link written fresh is caught");
        assert!(refusal(&format!("cat <<EOF | tee out.md\n{CARD}\nEOF\ngh pr comment 1 -F out.md"), &dir.join("sub")).is_some());
        // A `cd` before it moves where the file is read.
        std::fs::write(dir.join("sub/bare.md"), format!("see {CARD}\n")).unwrap();
        assert!(refusal("cd sub && gh issue comment 2 -F bare.md", &dir).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_quoted_as_code_is_not_a_paste() {
        assert!(!refused(&format!("gh pr comment 1 --body 'The guard refuses `{CARD}` bare.'")));
        assert!(!refused(&format!("gh pr comment 1 --body 'Example:\n```\nsee {CARD}\n```\nThat is all.'")));
        assert!(refused(&format!("gh pr comment 1 --body 'Example: `x`, then {CARD}'")), "a closed span ends where it closes");
        assert!(refused(&format!("gh pr comment 1 --body '```\ncode\n```\nsee {CARD}'")));
    }

    #[test]
    fn only_the_heredocs_feeding_the_gh_call_are_its_body() {
        let notes = format!("gh pr comment 1 --body \"Deployed $(date)\"\ncat > notes.md <<'EOF'\nartifact {CARD}\nEOF");
        assert!(!refused(&notes), "a heredoc for another file is not the comment");
        let multi = format!("gh pr create --title T \\\n  --body \"$(cat <<'EOF'\nShot: {CARD}\nEOF\n)\"");
        assert!(refused(&multi), "the body flag on a continuation line still opens it");
    }

    #[test]
    fn a_heredoc_for_another_command_is_not_the_body() {
        let commit = format!("git commit -F - <<'EOF'\nfix: see {CARD}\nEOF\ngh pr create --title T --body \"$(cat <<'EOF'\n{}\nEOF\n)\"", block("Fix", CARD));
        assert!(!refused(&commit), "the commit message is not the pull request's body");
    }

    #[test]
    fn a_body_file_is_matched_however_its_path_is_spelled() {
        let dir = std::env::temp_dir().join(format!("krowk-paste-guard-spelling-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body.md"), format!("see {CARD}\n")).unwrap();
        let ok = block("Cart", CARD);
        for (to, from) in [("\"body.md\"", "body.md"), ("./body.md", "body.md"), ("body.md", "./body.md"), ("'body.md'", "body.md")] {
            let cmd = format!("cat > {to} <<'EOF'\n{ok}\nEOF\ngh pr comment 1 -F {from}");
            assert!(refusal(&cmd, &dir).is_none(), "{cmd}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn many_unclosed_shifts_are_judged_quickly() {
        let body = "a <<b\n".repeat(30_000);
        let started = std::time::Instant::now();
        assert!(!refused(&format!("gh pr comment 1 --body '{body}'")));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn a_shift_in_a_quoted_body_is_not_a_heredoc() {
        assert!(refused(&format!("gh pr comment 1 --body 'Shift: a << b\nSee {CARD}'")));
    }

    #[test]
    fn global_repo_flags_and_stdin_by_path_are_followed() {
        assert!(refused(&format!("gh --repo o/r issue comment 1 --body '{CARD}'")));
        assert!(refused(&format!("gh -R o/r pr comment 1 -b '{CARD}'")));
        assert!(refused(&format!("gh pr comment 1 --body-file /dev/stdin <<EOF\nsee {CARD}\nEOF")));
    }

    #[test]
    fn punctuation_around_a_link_is_not_part_of_it() {
        let why = refusal(&format!("gh pr comment 1 --body 'Done: {CARD}.'"), Path::new("/")).unwrap();
        assert!(why.contains(&format!("({CARD})")), "{why}");
        assert!(refused(&format!("gh pr comment 1 --body '**{CARD}**'")));
    }

    #[test]
    fn a_long_line_of_near_links_is_judged_in_linear_time() {
        let body = ARTIFACT_PATH.repeat(150_000);
        let started = std::time::Instant::now();
        assert!(!refused(&format!("gh pr comment 1 --body '{body}'")));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
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
