//! A fix string, read for a person: one line per `; `-separated clause, the
//! command it names pulled onto a line of its own so it can be copied.

use regex_lite::Regex;
use std::sync::LazyLock;

#[derive(Debug, Default, PartialEq)]
pub struct FixLine {
    pub say: String,
    pub then: String,
    pub cmd: String,
}

pub fn fix_lines(fix: &str) -> Vec<FixLine> {
    fix.split("; ").map(fix_line).filter(|l| *l != FixLine::default()).collect()
}

/// One clause: what to know, what follows from it after an em dash, and the
/// backticked krowk command in it, which gets a line of its own. Words are
/// taken out only where nothing is lost: the lead-in that introduced the
/// command ("run", "try", a colon), and the command itself when all that
/// follows it is another way out (", or …", ", then …"), which is kept. A
/// command with more said around it leaves the sentence whole, and a clause
/// the command opens is about the command, so offers nothing to try.
fn fix_line(clause: &str) -> FixLine {
    static DANGLING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:(?:^|\s+)(?:run|try|use|with|pass|did you mean))?\s*[,:—]?\s*$").unwrap());
    static COMMAND: LazyLock<Regex> = LazyLock::new(|| Regex::new("`(krowk [^`]+)`").unwrap());
    let trimmed = clause.trim();
    let (say, then) = trimmed.split_once(" — ").unwrap_or((trimmed, ""));
    let whole = |cmd: String| FixLine { say: sentence(say), then: sentence(then), cmd };
    let Some(cmd) = COMMAND.captures(trimmed).map(|c| c[1].to_string()) else { return whole(String::new()) };
    let quoted = format!("`{cmd}`");
    let in_say = say.contains(quoted.as_str());
    let Some((before, after)) = (if in_say { say } else { then }).split_once(quoted.as_str()) else { return whole(cmd) };
    if in_say && before.trim().is_empty() {
        return whole(String::new());
    }
    let lead = DANGLING.replace_all(before.trim(), "").into_owned();
    let tail = after.strip_prefix(", ").filter(|t| t.starts_with("or ") || t.starts_with("then "));
    let bare = after.trim_matches(|c: char| c == '.' || c == '?' || c.is_whitespace()).is_empty();
    match (in_say, bare || tail.is_some()) {
        (true, true) => FixLine { say: sentence(&lead), then: sentence(tail.unwrap_or(then)), cmd },
        (false, true) if lead.is_empty() => FixLine { say: sentence(say), then: sentence(tail.unwrap_or("")), cmd },
        _ => whole(cmd),
    }
}

/// The first letter in upper case, the rest as it was.
pub fn capitalised(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return String::new() };
    first.to_uppercase().chain(chars).collect()
}

/// Capitalised and closed with a full stop, unless it already ends in one.
pub fn sentence(s: &str) -> String {
    let mut out = capitalised(s.trim());
    if out.is_empty() {
        return out;
    }
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_in_a_clause_gets_its_own_line() {
        let lines = fix_lines("no key to verify — run `krowk login --token krowk_sk_...`, or upload anonymously");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].say, "No key to verify.");
        assert_eq!(lines[0].cmd, "krowk login --token krowk_sk_...");
        assert_eq!(lines[0].then, "Or upload anonymously.");
        let mid = fix_lines("-p needs a prompt: `krowk -p \"hi\"`, or pipe one in");
        assert_eq!((mid[0].say.as_str(), mid[0].then.as_str()), ("-p needs a prompt.", "Or pipe one in."));
        let lead = fix_lines("run `krowk login`, or upload anonymously");
        assert_eq!((lead[0].say.as_str(), lead[0].cmd.as_str()), ("", "krowk login"));
        let alone = fix_lines("unknown flag --nope; run `krowk help push`");
        assert_eq!((alone[1].say.as_str(), alone[1].cmd.as_str()), ("", "krowk help push"));
        let two = fix_lines("first thing; then run `krowk runs finish run_x`");
        assert_eq!((two[0].say.as_str(), two[1].say.as_str(), two[1].cmd.as_str()), ("First thing.", "Then.", "krowk runs finish run_x"));
        // The command as the subject: the sentence stays, with nothing to try.
        let subject = fix_lines("`krowk login` takes no arguments — the key goes behind --token");
        assert_eq!(subject[0], FixLine { say: "`krowk login` takes no arguments.".into(), then: "The key goes behind --token.".into(), cmd: String::new() });
        // More said after the command than a way out: the sentence stays whole.
        let more = fix_lines("no default — run `krowk workspaces use <name>` to point it at a key, or `krowk login`");
        assert_eq!((more[0].say.as_str(), more[0].cmd.as_str()), ("No default.", "krowk workspaces use <name>"));
        let guess = fix_lines("`pussh` is not a krowk command — did you mean `krowk push`?");
        assert_eq!((guess[0].then.as_str(), guess[0].cmd.as_str()), ("", "krowk push"));
        let ci = fix_lines("needs a person — pass `krowk login --token krowk_sk_...`, or add --no-browser");
        assert_eq!((ci[0].then.as_str(), ci[0].cmd.as_str()), ("Or add --no-browser.", "krowk login --token krowk_sk_..."));
        assert_eq!(more[0].then, "Run `krowk workspaces use <name>` to point it at a key, or `krowk login`.");
    }
}
