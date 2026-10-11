//! `write`, and the two string-replacement edit tools: `str_replace`
//! (Claude's names) and `search_replace` (Grok's). Both replace an exact
//! run of text that must occur once, unless every occurrence is asked for:
//! a replacement that could land in two places is a guess, and a guess
//! that lands wrong is worse than a refusal the model reads and fixes.

use super::{Scope, open_regular, write_atomic};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

/// A file an edit reads is at most this big: past it, a whole-file rewrite
/// in memory is not an edit any more.
const EDIT_MAX_BYTES: u64 = 16 << 20;
/// Lines of the result shown above and below a replacement, so the model
/// sees what the file now says without reading it again.
const SNIPPET_CONTEXT: usize = 3;
const SNIPPET_MAX_LINES: usize = 40;

/// Write a file, replacing it whole.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    /// The file to write; missing parent directories are created.
    pub path: String,
    /// The file's entire new content.
    pub content: String,
}

/// Replace text in a file.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrReplaceInput {
    /// The file to edit.
    pub path: String,
    /// The exact text to replace, whitespace and indentation included. It must occur exactly once unless `replace_all` is set.
    pub old_str: String,
    /// The text to put in its place.
    pub new_str: String,
    /// Replace every occurrence rather than requiring exactly one.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

/// Replace text in a file.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchReplaceInput {
    /// The file to edit.
    pub file_path: String,
    /// The exact text to search for, whitespace and indentation included. It must occur exactly once unless `replace_all` is set.
    pub old_string: String,
    /// The text to replace it with.
    pub new_string: String,
    /// Replace every occurrence rather than requiring exactly one.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

pub(super) fn write(i: &WriteInput, scope: &Scope) -> (String, bool) {
    let path = match scope.edit_path(&i.path) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };
    let existed = match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => true,
        Ok(_) => return (format!("{} is not a regular file (a directory, a device or a pipe), which write does not replace", path.display()), true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return (format!("{} could not be written: {e}", path.display()), true),
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return (format!("{} could not be created: {e}", parent.display()), true);
    }
    if let Err(e) = write_atomic(&path, i.content.as_bytes(), scope.sandbox.is_some()) {
        return (format!("{} could not be written: {e}", path.display()), true);
    }
    let lines = i.content.lines().count();
    (format!("{} {} ({lines} lines, {} bytes)", if existed { "wrote" } else { "created" }, path.display(), i.content.len()), false)
}

/// What `str_replace` and `search_replace` share, under the argument names
/// the calling tool uses in its messages.
pub(super) struct Replace<'a> {
    pub tool: &'static str,
    pub old_name: &'static str,
    pub path: &'a str,
    pub old: &'a str,
    pub new: &'a str,
    pub replace_all: bool,
}

/// Reads a file an edit may change: regular, not too big, UTF-8, and not
/// binary — an edit of bytes it cannot show would be blind.
pub(super) fn read_text(path: &Path, tool: &str, exact: bool) -> Result<String, String> {
    use std::io::Read;
    let (f, size) = open_regular(path, exact).map_err(|e| e.replace("which read does not open", &format!("which {tool} does not edit")))?;
    if size > EDIT_MAX_BYTES {
        return Err(format!("{} is {} MB, more than {tool} edits ({} MB) — change it with bash instead", path.display(), size >> 20, EDIT_MAX_BYTES >> 20));
    }
    let mut raw = Vec::with_capacity(size as usize);
    f.take(EDIT_MAX_BYTES + 1).read_to_end(&mut raw).map_err(|e| format!("{} could not be read: {e}", path.display()))?;
    if raw.contains(&0) {
        return Err(format!("{} is a binary file, which {tool} does not edit", path.display()));
    }
    String::from_utf8(raw).map_err(|_| format!("{} is not UTF-8 text, which {tool} does not edit — change it with bash instead", path.display()))
}

pub(super) fn replace(r: &Replace<'_>, scope: &Scope) -> (String, bool) {
    let path = match scope.edit_path(r.path) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };
    if r.old.is_empty() {
        return (format!("{} is empty — say which text to replace; to create a file, use write", r.old_name), true);
    }
    if r.old == r.new {
        return (format!("{} and its replacement are the same, so there is nothing to change", r.old_name), true);
    }
    let text = match read_text(&path, r.tool, scope.sandbox.is_some()) {
        Ok(t) => t,
        Err(e) => return (e, true),
    };
    // A file with CRLF endings and a model that sends LF: the same text,
    // matched in the file's own line endings.
    let crlf = text.contains("\r\n") && !r.old.contains('\r');
    let (old, new) = if crlf && !text.contains(r.old) { (r.old.replace('\n', "\r\n"), r.new.replace('\n', "\r\n")) } else { (r.old.to_string(), r.new.to_string()) };
    let at: Vec<usize> = text.match_indices(old.as_str()).map(|(i, _)| i).collect();
    let line_of = |byte: usize| text[..byte].matches('\n').count() + 1;
    match at.len() {
        0 => {
            let hint = match loose_match(&text, r.old) {
                Some(line) => format!(" Text that differs from it only in whitespace starts at line {line}: copy it exactly as read shows it."),
                None => " The file may have changed since you read it — read it again and copy the text exactly, whitespace and indentation included.".into(),
            };
            return (format!("{} was not found in {}.{hint}", r.old_name, path.display()), true);
        }
        1 => {}
        n if !r.replace_all => {
            let lines: Vec<String> = at.iter().take(10).map(|&b| line_of(b).to_string()).collect();
            return (
                format!(
                    "{} occurs {n} times in {} (at lines {}{}) — include more of the surrounding lines so it matches exactly once, or set replace_all to change every one",
                    r.old_name,
                    path.display(),
                    lines.join(", "),
                    if n > 10 { ", …" } else { "" }
                ),
                true,
            );
        }
        _ => {}
    }
    let first_line = line_of(at[0]);
    let updated = if r.replace_all { text.replace(old.as_str(), &new) } else { text.replacen(old.as_str(), &new, 1) };
    if let Err(e) = write_atomic(&path, updated.as_bytes(), scope.sandbox.is_some()) {
        return (format!("{} could not be written: {e}", path.display()), true);
    }
    let what = if at.len() == 1 { "1 occurrence".to_string() } else { format!("{} occurrences", at.len()) };
    let shown_lines = new.matches('\n').count() + 1;
    (format!("edited {}: replaced {what}. It now reads:\n{}", path.display(), snippet(&updated, first_line, shown_lines)), false)
}

/// Where `old` would match if whitespace at the ends of each line did not
/// count: the line it starts on. Only a hint — the edit itself stays exact.
fn loose_match(text: &str, old: &str) -> Option<usize> {
    let want: Vec<&str> = old.lines().map(str::trim).collect();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if want.is_empty() || want.iter().all(|l| l.is_empty()) || want.len() > lines.len() {
        return None;
    }
    (0..=lines.len() - want.len()).find(|&i| lines[i..i + want.len()] == want[..]).map(|i| i + 1)
}

/// The replaced lines with a little context, numbered as read numbers them.
pub(super) fn snippet(text: &str, first: usize, count: usize) -> String {
    let start = first.saturating_sub(SNIPPET_CONTEXT).max(1);
    let end = (first + count.max(1) - 1 + SNIPPET_CONTEXT).min(start + SNIPPET_MAX_LINES - 1);
    let mut out = String::new();
    for (n, line) in text.lines().enumerate().map(|(i, l)| (i + 1, l)).skip(start - 1).take(end + 1 - start) {
        out += &format!("{n:>6}\t{}\n", line.trim_end_matches('\r'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::dir;
    use super::super::*;
    use serde_json::json;

    fn env(d: &Path, edit: EditTool) -> ToolEnv<'_> {
        ToolEnv { cwd: d, permission_mode: PermissionMode::AcceptEdits, edit, evidence: None, builds: None, live: None, env: &[], jobs: None, steers: None }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_1_write_creates_and_replaces_files_and_needs_edit_permission() {
        let d = dir("write");
        let e = env(&d, EditTool::StrReplace);
        let (out, err) = run(WRITE, &json!({"path": "src/new.txt", "content": "a\nb\n"}), &e).await;
        assert!(!err && out.starts_with("created ") && out.contains("2 lines"), "{out}");
        assert_eq!(std::fs::read_to_string(d.join("src/new.txt")).unwrap(), "a\nb\n");
        let (out, err) = run(WRITE, &json!({"path": "src/new.txt", "content": "c"}), &e).await;
        assert!(!err && out.starts_with("wrote "), "{out}");
        assert_eq!(std::fs::read_to_string(d.join("src/new.txt")).unwrap(), "c");
        assert!(run(WRITE, &json!({"path": "src", "content": "x"}), &e).await.0.contains("not a regular file"));
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            let refused = run(WRITE, &json!({"path": "x", "content": "x"}), &ToolEnv { cwd: &d, permission_mode: mode, edit: EditTool::StrReplace, evidence: None, builds: None, live: None, env: &[], jobs: None, steers: None }).await;
            let why = if mode == PermissionMode::Plan { "plan mode" } else { "acceptEdits" };
            assert!(refused.1 && refused.0.contains(why), "{refused:?}");
        }
        assert!(!d.join("x").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_2_str_replace_edits_a_unique_match_and_refuses_the_rest() {
        let d = dir("str-replace");
        let e = env(&d, EditTool::StrReplace);
        std::fs::write(d.join("a.rs"), "fn main() {\n    let x = 1;\n    let y = 1;\n}\n").unwrap();
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "let x = 1;", "new_str": "let x = 2;"}), &e).await;
        assert!(!err, "{out}");
        assert!(out.contains("replaced 1 occurrence") && out.contains("     2\t    let x = 2;"), "the result shows the edited lines: {out}");
        assert_eq!(std::fs::read_to_string(d.join("a.rs")).unwrap(), "fn main() {\n    let x = 2;\n    let y = 1;\n}\n");

        // Non-unique: refused with where the matches are, the file untouched.
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": " = ", "new_str": " := "}), &e).await;
        assert!(err && out.contains("occurs 2 times") && out.contains("lines 2, 3"), "{out}");
        assert!(std::fs::read_to_string(d.join("a.rs")).unwrap().contains("let x = 2;"));
        // Unless every occurrence is asked for.
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": " = ", "new_str": " := ", "replace_all": true}), &e).await;
        assert!(!err && out.contains("2 occurrences"), "{out}");

        // Stale context: the text is not there any more.
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "let x = 2;", "new_str": "let x = 3;"}), &e).await;
        assert!(err && out.contains("was not found") && out.contains("read it again"), "{out}");
        // Close, but for whitespace: the hint names the line.
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "let x := 2;\nlet y := 1;", "new_str": "z"}), &e).await;
        assert!(err && out.contains("only in whitespace starts at line 2"), "{out}");

        assert!(run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "", "new_str": "x"}), &e).await.0.contains("use write"));
        assert!(run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "x", "new_str": "x"}), &e).await.0.contains("nothing to change"));
        assert!(run(STR_REPLACE, &json!({"path": "missing.rs", "old_str": "x", "new_str": "y"}), &e).await.0.contains("does not exist"));
        std::fs::write(d.join("bin"), [b'x', 0, b'y']).unwrap();
        assert!(run(STR_REPLACE, &json!({"path": "bin", "old_str": "x", "new_str": "y"}), &e).await.0.contains("binary"));
        // Refused where edits are not allowed.
        let refused = run(STR_REPLACE, &json!({"path": "a.rs", "old_str": "fn", "new_str": "pub fn"}), &ToolEnv { permission_mode: PermissionMode::Default, ..e }).await;
        assert!(refused.1 && refused.0.contains("acceptEdits"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn str_replace_keeps_a_crlf_files_line_endings() {
        let d = dir("crlf");
        std::fs::write(d.join("w.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        let (out, err) = run(STR_REPLACE, &json!({"path": "w.txt", "old_str": "one\ntwo", "new_str": "one\n2"}), &env(&d, EditTool::StrReplace)).await;
        assert!(!err, "{out}");
        assert_eq!(std::fs::read_to_string(d.join("w.txt")).unwrap(), "one\r\n2\r\nthree\r\n");
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_2_search_replace_speaks_groks_names_with_the_same_rules() {
        let d = dir("search-replace");
        let e = env(&d, EditTool::SearchReplace);
        std::fs::write(d.join("c.py"), "a = 1\nb = 1\n").unwrap();
        let (out, err) = run(SEARCH_REPLACE, &json!({"file_path": "c.py", "old_string": "a = 1", "new_string": "a = 2"}), &e).await;
        assert!(!err, "{out}");
        assert_eq!(std::fs::read_to_string(d.join("c.py")).unwrap(), "a = 2\nb = 1\n");
        let (out, err) = run(SEARCH_REPLACE, &json!({"file_path": "c.py", "old_string": "= ", "new_string": "== "}), &e).await;
        assert!(err && out.starts_with("old_string occurs 2 times"), "the error names the argument as Grok sent it: {out}");
        let (out, err) = run(SEARCH_REPLACE, &json!({"file_path": "c.py", "old_string": "c = 3", "new_string": "c = 4"}), &e).await;
        assert!(err && out.starts_with("old_string was not found"), "{out}");
        // Claude's argument names are not Grok's.
        assert!(run(SEARCH_REPLACE, &json!({"path": "c.py", "old_str": "a", "new_str": "b"}), &e).await.0.contains("invalid input"));
        let _ = std::fs::remove_dir_all(d);
    }
}
