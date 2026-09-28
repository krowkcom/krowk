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
/// backticked krowk command in it, when it has one — in which case the words
/// that only introduced the command ("run", "try", a trailing colon) go.
fn fix_line(clause: &str) -> FixLine {
    static DANGLING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\s+(?:run|try|use|with))?\s*[,:—]?\s*$").unwrap());
    static COMMAND: LazyLock<Regex> = LazyLock::new(|| Regex::new("`(krowk [^`]+)`").unwrap());
    let cmd = COMMAND.captures(clause).map(|c| c[1].to_string()).unwrap_or_default();
    let trimmed = clause.trim();
    let (mut say, mut then) = match trimmed.split_once(" — ") {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (trimmed.to_string(), String::new()),
    };
    if !cmd.is_empty() {
        let quoted = format!("`{cmd}`");
        let s = say.trim();
        say = DANGLING.replace_all(s.strip_suffix(quoted.as_str()).unwrap_or(s), "").into_owned();
        then.clear();
    }
    FixLine { say: sentence(&say), then: sentence(&then), cmd }
}

/// Capitalised and closed with a full stop, unless it already ends in one.
pub fn sentence(s: &str) -> String {
    let s = s.trim();
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut out: String = if first.is_lowercase() { first.to_uppercase().collect() } else { first.to_string() };
    out.push_str(chars.as_str());
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
        let two = fix_lines("first thing; then run `krowk runs finish run_x`");
        assert_eq!((two[0].say.as_str(), two[1].say.as_str(), two[1].cmd.as_str()), ("First thing.", "Then.", "krowk runs finish run_x"));
    }
}
