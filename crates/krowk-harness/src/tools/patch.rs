//! `apply_patch`: the V4A patch envelope GPT and Codex models are trained
//! to write.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path/new.txt
//! +its first line
//! *** Update File: path/old.rs
//! *** Move to: path/renamed.rs
//! @@ fn main() {
//!      let unchanged = 1;
//! -    let old = 2;
//! +    let new = 2;
//! *** Delete File: path/gone.txt
//! *** End Patch
//! ```
//!
//! A chunk's context is found in the file rather than trusted to a line
//! number, first exactly, then ignoring trailing whitespace, then ignoring
//! whitespace at both ends, then with typographic punctuation folded to
//! ASCII — the leniency Codex's own applier has, so a patch that applies
//! there applies here. The patch is all or nothing: every file is worked
//! out in memory first, and nothing is written unless all of it applies.
//!
//! Offered as a freeform tool with the Lark grammar below where the wire API
//! takes grammar tools, and as a JSON function tool with one `input` string
//! elsewhere; the runner takes either shape.

use super::edit::read_text;
use super::{Scope, commit, stage};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The grammar a freeform `apply_patch` is constrained to: Codex's.
pub const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// Apply a patch.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchInput {
    /// The whole patch, from `*** Begin Patch` to `*** End Patch`.
    pub input: String,
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

#[derive(Debug, PartialEq)]
enum Hunk {
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, chunks: Vec<Chunk> },
}

#[derive(Debug, Default, PartialEq)]
struct Chunk {
    /// The `@@` line's text: a line to find before the chunk's lines.
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    /// The chunk ends at the end of the file.
    eof: bool,
}

/// Parses the envelope. Errors name the line, counting from 1.
fn parse(patch: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    // A model that wraps the patch in the shell heredoc it saw in training
    // (`apply_patch <<'EOF'` … `EOF`) means the patch inside.
    let lines = match lines.as_slice() {
        [first, inner @ .., last] if first.contains("<<") && !first.starts_with("***") && last.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => inner,
        all => all,
    };
    match (lines.first().map(|l| l.trim()), lines.last().map(|l| l.trim())) {
        (Some(BEGIN), Some(END)) if lines.len() >= 2 => {}
        (Some(BEGIN), _) => return Err(format!("the patch does not end with `{END}`")),
        _ => return Err(format!("the patch does not start with `{BEGIN}`")),
    }
    let body = &lines[1..lines.len() - 1];
    let mut hunks = Vec::new();
    let mut i = 0;
    let at = |i: usize| i + 2;
    // A hunk's body ends at the first line whose first character is `*`,
    // as in Codex: a context line starts with a space, so ` *** Add File:`
    // is text in the file, never a header. The header line itself is read
    // with the whitespace around it trimmed.
    let header = |l: &str| l.starts_with('*') && l.trim() != EOF;
    while i < body.len() {
        let line = body[i].trim();
        if let Some(path) = line.strip_prefix(ADD) {
            i += 1;
            let mut added = Vec::new();
            while i < body.len() && !header(body[i]) {
                match body[i].strip_prefix('+') {
                    Some(l) => added.push(l.to_string()),
                    None => return Err(format!("line {}: every line of an added file starts with `+`, and {:?} does not", at(i), body[i])),
                }
                i += 1;
            }
            hunks.push(Hunk::Add { path: path.trim().into(), lines: added });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete { path: path.trim().into() });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let start = i;
            i += 1;
            let mut move_to = None;
            if let Some(to) = body.get(i).and_then(|l| l.trim().strip_prefix(MOVE)) {
                move_to = Some(to.trim().to_string());
                i += 1;
            }
            let mut chunks: Vec<Chunk> = Vec::new();
            while i < body.len() && !header(body[i]) {
                let l = body[i].trim_end_matches('\r');
                if l == "@@" || l.starts_with("@@ ") {
                    let ctx = l.strip_prefix("@@").unwrap_or_default().trim();
                    chunks.push(Chunk { context: (!ctx.is_empty()).then(|| ctx.to_string()), ..Chunk::default() });
                } else if l.trim() == EOF {
                    match chunks.last_mut() {
                        Some(c) => c.eof = true,
                        None => return Err(format!("line {}: `{EOF}` before any change", at(i))),
                    }
                } else {
                    // The first chunk may start without an `@@`; a chunk
                    // that ended at the end of the file ends there.
                    if chunks.last().is_none_or(|c| c.eof) {
                        chunks.push(Chunk::default());
                    }
                    let c = chunks.last_mut().expect("just pushed");
                    match l.chars().next() {
                        Some(' ') => {
                            c.old.push(l[1..].into());
                            c.new.push(l[1..].into());
                        }
                        // An empty line is an empty context line: models
                        // drop the lone space more often than not.
                        None => {
                            c.old.push(String::new());
                            c.new.push(String::new());
                        }
                        Some('-') => c.old.push(l[1..].into()),
                        Some('+') => c.new.push(l[1..].into()),
                        _ => return Err(format!("line {}: {l:?} is not a change — a changed file's lines start with ` `, `-`, `+` or `@@`", at(i))),
                    }
                }
                i += 1;
            }
            chunks.retain(|c| !(c.old.is_empty() && c.new.is_empty() && c.context.is_none()));
            if chunks.is_empty() && move_to.is_none() {
                return Err(format!("line {}: `{UPDATE}{}` changes nothing", at(start), path.trim()));
            }
            hunks.push(Hunk::Update { path: path.trim().into(), move_to, chunks });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!("line {}: {line:?} is not a hunk header — one of `{ADD}`, `{DELETE}` or `{UPDATE}` followed by a path", at(i)));
        }
    }
    if hunks.is_empty() {
        return Err("the patch has no hunks".into());
    }
    Ok(hunks)
}

/// Typographic punctuation a model may write where the file has ASCII.
fn fold(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

/// Where `pattern` occurs in `lines` at or after `start`, by the loosest
/// comparison that finds it; at the end of the file first when `eof`.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    type Eq = dyn Fn(&str, &str) -> bool;
    let passes: [&Eq; 4] = [&|a, b| a == b, &|a, b| a.trim_end() == b.trim_end(), &|a, b| a.trim() == b.trim(), &|a, b| fold(a) == fold(b)];
    for eq in passes {
        let hit = |i: usize| lines[i..i + pattern.len()].iter().zip(pattern).all(|(a, b)| eq(a, b));
        if eof && last >= start && hit(last) {
            return Some(last);
        }
        if let Some(i) = (start..=last).find(|&i| hit(i)) {
            return Some(i);
        }
    }
    None
}

/// A file's lines, and whether they end in CRLF: kept, so an edited
/// Windows file stays one.
fn split(text: &str) -> (Vec<String>, bool) {
    let crlf = text.contains("\r\n");
    let mut lines: Vec<String> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    (lines, crlf)
}

fn join(lines: &[String], crlf: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let nl = if crlf { "\r\n" } else { "\n" };
    lines.join(nl) + nl
}

/// Applies an update's chunks to a file's lines.
fn update(path: &Path, lines: Vec<String>, chunks: &[Chunk]) -> Result<Vec<String>, String> {
    let mut edits: Vec<(usize, usize, &[String])> = Vec::new();
    let mut at = 0usize;
    for (n, c) in chunks.iter().enumerate() {
        let n = n + 1;
        if let Some(ctx) = &c.context {
            match seek(&lines, std::slice::from_ref(ctx), at, false) {
                Some(i) => at = i + 1,
                None => {
                    return Err(format!(
                        "chunk {n} of {}: its context line `@@ {ctx}` is not in the file — it may have changed since you read it; read it again and regenerate the patch",
                        path.display()
                    ));
                }
            }
        }
        if c.old.is_empty() {
            // A pure insertion: after its context line when it has one,
            // else at the end of the file.
            let i = if c.context.is_some() { at } else { lines.len() };
            edits.push((i, 0, &c.new));
            continue;
        }
        let mut old: &[String] = &c.old;
        let mut new: &[String] = &c.new;
        let mut found = seek(&lines, old, at, c.eof);
        // A trailing empty line in the chunk is often the file's final
        // newline rather than a line of its own.
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(&lines, old, at, c.eof);
        }
        let Some(i) = found else {
            let shown: Vec<&str> = c.old.iter().take(3).map(String::as_str).collect();
            return Err(format!(
                "chunk {n} of {}: the lines it changes are not in the file{} (starting {:?}) — it may have changed since you read it; read it again and regenerate the patch",
                path.display(),
                if at > 0 { " after its context" } else { "" },
                shown.join("\n")
            ));
        };
        edits.push((i, old.len(), new));
        at = i + old.len();
    }
    // Applied from the bottom up, so each edit's position still holds; an
    // insertion at the end may have been found before an earlier change.
    edits.sort_by_key(|e| e.0);
    let mut lines = lines;
    for (i, len, new) in edits.into_iter().rev() {
        lines.splice(i..i + len, new.iter().cloned());
    }
    Ok(lines)
}

/// The patch's input, however the call carried it: a freeform call's text,
/// or a function call's `input` string.
pub(super) fn input_text(input: &serde_json::Value) -> Result<String, String> {
    match input {
        serde_json::Value::String(s) => Ok(s.clone()),
        v => ApplyPatchInput::deserialize(v).map(|i| i.input).map_err(|e| e.to_string()),
    }
}

/// Every path a patch names — each hunk's, and a move's destination —
/// for the permission evaluator, which judges the patch by all of them.
pub(super) fn paths(patch: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for h in parse(patch)? {
        match h {
            Hunk::Add { path, .. } | Hunk::Delete { path } => out.push(path),
            Hunk::Update { path, move_to, .. } => {
                out.push(path);
                out.extend(move_to);
            }
        }
    }
    Ok(out)
}

pub(super) fn apply(patch: &str, scope: &Scope) -> (String, bool) {
    let hunks = match parse(patch) {
        Ok(h) => h,
        Err(e) => return (format!("the patch did not parse, so nothing was changed: {e}"), true),
    };
    // Every file's end state, worked out before anything is written; None
    // is deleted. A file the patch touches twice sees its first change.
    let mut files: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut summary = Vec::new();
    let exact = scope.sandbox.is_some();
    let current = |files: &BTreeMap<PathBuf, Option<String>>, p: &Path| -> Result<Option<String>, String> {
        match files.get(p) {
            Some(state) => Ok(state.clone()),
            None if p.exists() => read_text(p, "apply_patch", exact).map(Some),
            None => Ok(None),
        }
    };
    let fail = |e: String| (format!("the patch does not apply, so nothing was changed: {e}"), true);
    // Every path the patch names, Move targets included, is held to the
    // same scope as any other file tool.
    let at = |path: &str| scope.edit_path(path);
    for h in &hunks {
        match h {
            Hunk::Add { path, lines } => {
                let p = match at(path) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                if files.get(&p).is_some_and(Option::is_some) || (!files.contains_key(&p) && p.exists()) {
                    return fail(format!("{} already exists — change it with `{UPDATE}{path}`", p.display()));
                }
                files.insert(p, Some(join(lines, false)));
                summary.push(format!("A {path}"));
            }
            Hunk::Delete { path } => {
                let p = match at(path) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                // Deleted unread: a binary or a huge file goes as readily as
                // any other.
                let exists = match files.get(&p) {
                    Some(state) => state.is_some(),
                    None => std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() || m.file_type().is_symlink()),
                };
                if !exists {
                    return fail(format!("{} does not exist as a file, so it cannot be deleted", p.display()));
                }
                files.insert(p, None);
                summary.push(format!("D {path}"));
            }
            Hunk::Update { path, move_to, chunks } => {
                let p = match at(path) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                let text = match current(&files, &p) {
                    Ok(Some(t)) => t,
                    Ok(None) => return fail(format!("{} does not exist — create it with `{ADD}{path}`", p.display())),
                    Err(e) => return fail(e),
                };
                let (lines, crlf) = split(&text);
                let updated = match update(&p, lines, chunks) {
                    Ok(l) => join(&l, crlf),
                    Err(e) => return fail(e),
                };
                match move_to {
                    Some(to) => {
                        let dst = match at(to) {
                            Ok(p) => p,
                            Err(e) => return fail(e),
                        };
                        if dst != p && (files.get(&dst).is_some_and(Option::is_some) || (!files.contains_key(&dst) && dst.exists())) {
                            return fail(format!("{} already exists, so {path} cannot be moved there", dst.display()));
                        }
                        files.insert(p, None);
                        files.insert(dst, Some(updated));
                        summary.push(format!("M {path} -> {to}"));
                    }
                    None => {
                        files.insert(p, Some(updated));
                        summary.push(format!("M {path}"));
                    }
                }
            }
        }
    }
    // Written only now that all of it applies. Deletions last, so a move
    // whose write fails leaves the original in place.
    // Every new content is staged in a temporary file beside its target
    // first; only when all are staged is each renamed into place, so a full
    // disk or a read-only directory fails the patch with nothing changed.
    let (writes, deletes): (Vec<_>, Vec<_>) = files.into_iter().partition(|(_, s)| s.is_some());
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let unstage = |staged: &[(PathBuf, PathBuf)]| {
        for (tmp, _) in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for (p, text) in writes {
        let text = text.expect("partitioned");
        if let Some(parent) = p.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            unstage(&staged);
            return (format!("{} could not be created, so nothing was changed: {e}", parent.display()), true);
        }
        match stage(&p, text.as_bytes(), exact) {
            Ok(tmp) => staged.push((tmp, p)),
            Err(e) => {
                unstage(&staged);
                return (format!("{} could not be written, so nothing was changed: {e}", p.display()), true);
            }
        }
    }
    for (n, (tmp, p)) in staged.iter().enumerate() {
        if let Err(e) = commit(tmp, p, exact) {
            unstage(&staged[n + 1..]);
            return (format!("{} could not be written: {e} — the patch is partly applied; read the files it names before patching again", p.display()), true);
        }
    }
    for (p, _) in deletes {
        if let Err(e) = super::remove(&p, exact)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return (format!("{} could not be deleted: {e} — the rest of the patch is applied", p.display()), true);
        }
    }
    (format!("Success. Updated the following files:\n{}", summary.join("\n")), false)
}

#[cfg(test)]
mod tests {
    use super::super::tests::dir;
    use super::super::*;
    use super::*;
    use serde_json::json;

    fn env(d: &Path) -> ToolEnv<'_> {
        ToolEnv { cwd: d, permission_mode: PermissionMode::AcceptEdits, edit: EditTool::ApplyPatch, evidence: None, builds: None, live: None, env: &[] }
    }

    #[test]
    fn the_envelope_parses_every_hunk_kind() {
        let hunks = parse(
            "*** Begin Patch\n*** Add File: n.txt\n+hi\n*** Delete File: old.txt\n*** Update File: a.rs\n*** Move to: b.rs\n@@ fn main() {\n     keep\n-    old\n+    new\n\n@@\n-tail\n+end\n*** End of File\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0], Hunk::Add { path: "n.txt".into(), lines: vec!["hi".into()] });
        assert_eq!(hunks[1], Hunk::Delete { path: "old.txt".into() });
        let Hunk::Update { path, move_to, chunks } = &hunks[2] else { panic!() };
        assert_eq!((path.as_str(), move_to.as_deref()), ("a.rs", Some("b.rs")));
        assert_eq!(chunks[0].context.as_deref(), Some("fn main() {"));
        assert_eq!(chunks[0].old, ["    keep", "    old", ""]);
        assert_eq!(chunks[0].new, ["    keep", "    new", ""]);
        assert!(chunks[1].eof && chunks[1].context.is_none());
        // A header line indented or trailed by whitespace is still a header,
        // as Codex reads it; inside a hunk's body only a line starting `*`
        // ends it, so an indented `*** …` there is a context line.
        let indented = parse("*** Begin Patch\n  *** Update File: a.rs  \n-x\n+y\n*** Delete File: b.rs  \n  *** Add File: c.txt\n+c\n *** End Patch").unwrap();
        assert_eq!(indented.len(), 3);
        assert!(matches!(&indented[0], Hunk::Update { path, .. } if path == "a.rs"));
        assert_eq!(indented[1], Hunk::Delete { path: "b.rs".into() });
        assert_eq!(indented[2], Hunk::Add { path: "c.txt".into(), lines: vec!["c".into()] });
        let context = parse("*** Begin Patch\n*** Update File: notes.md\n *** Add File: x\n-old\n+new\n*** End Patch").unwrap();
        let Hunk::Update { chunks, .. } = &context[0] else { panic!() };
        assert_eq!(chunks[0].old, ["*** Add File: x", "old"], "a context line that looks like a header is context");
        // Wrapped in the heredoc a model saw in training, it means the same.
        assert_eq!(parse("apply_patch <<'EOF'\n*** Begin Patch\n*** Delete File: x\n*** End Patch\nEOF").unwrap(), vec![Hunk::Delete { path: "x".into() }]);
        for (bad, says) in [
            ("*** Delete File: x\n*** End Patch", "does not start"),
            ("*** Begin Patch\n*** Delete File: x", "does not end"),
            ("*** Begin Patch\n*** End Patch", "no hunks"),
            ("*** Begin Patch\n*** Frobnicate: x\n*** End Patch", "line 2"),
            ("*** Begin Patch\n*** Update File: x\n*** End Patch", "changes nothing"),
            ("*** Begin Patch\n*** Update File: x\n@@\n?what\n*** End Patch", "line 4"),
            ("*** Begin Patch\n*** Add File: x\nno plus\n*** End Patch", "starts with `+`"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(says), "{bad:?}: {e}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_2_apply_patch_adds_updates_moves_and_deletes_in_one_go() {
        let d = dir("patch");
        std::fs::write(d.join("a.rs"), "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n\nfn other() {\n    let x = 1;\n}\n").unwrap();
        std::fs::write(d.join("gone.txt"), "bye\n").unwrap();
        let patch = "*** Begin Patch\n\
                     *** Add File: new/n.txt\n+hello\n+world\n\
                     *** Update File: a.rs\n*** Move to: b.rs\n@@ fn other() {\n-    let x = 1;\n+    let x = 2;\n\
                     *** Delete File: gone.txt\n\
                     *** End Patch";
        // The function-tool shape: the patch in `input`.
        let (out, err) = run(APPLY_PATCH, &json!({ "input": patch }), &env(&d)).await;
        assert!(!err, "{out}");
        assert_eq!(out, "Success. Updated the following files:\nA new/n.txt\nM a.rs -> b.rs\nD gone.txt");
        assert_eq!(std::fs::read_to_string(d.join("new/n.txt")).unwrap(), "hello\nworld\n");
        // The @@ context picked the second `let x = 1;`, not the first.
        assert_eq!(std::fs::read_to_string(d.join("b.rs")).unwrap(), "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n\nfn other() {\n    let x = 2;\n}\n");
        assert!(!d.join("a.rs").exists() && !d.join("gone.txt").exists());
        // A file whose text looks like a patch header is patched like any other.
        std::fs::write(d.join("notes.md"), "*** Add File: x\nold\n").unwrap();
        let (out, err) = run(APPLY_PATCH, &json!("*** Begin Patch\n*** Update File: notes.md\n *** Add File: x\n-old\n+new\n*** End Patch"), &env(&d)).await;
        assert!(!err, "{out}");
        assert_eq!(std::fs::read_to_string(d.join("notes.md")).unwrap(), "*** Add File: x\nnew\n");
        // A file too binary to edit is still deletable: a Delete never reads it.
        std::fs::write(d.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        let (out, err) = run(APPLY_PATCH, &json!("*** Begin Patch\n*** Delete File: blob.bin\n*** End Patch"), &env(&d)).await;
        assert!(!err && !d.join("blob.bin").exists(), "{out}");

        // The freeform shape: the call's input is the patch itself. Lines
        // that differ only in trailing whitespace and typographic quotes
        // still match; the file keeps its CRLF endings.
        std::fs::write(d.join("w.txt"), "say \"hi\"   \r\nend\r\n").unwrap();
        let (out, err) = run(APPLY_PATCH, &json!("*** Begin Patch\n*** Update File: w.txt\n-say \u{201C}hi\u{201D}\n+say \"hello\"\n end\n*** End Patch\n"), &env(&d)).await;
        assert!(!err, "{out}");
        assert_eq!(std::fs::read_to_string(d.join("w.txt")).unwrap(), "say \"hello\"\r\nend\r\n");

        // A pure insertion after its context line, and one at the end.
        std::fs::write(d.join("i.txt"), "a\nb\n").unwrap();
        let (out, err) = run(APPLY_PATCH, &json!("*** Begin Patch\n*** Update File: i.txt\n@@ a\n+a2\n@@\n+z\n*** End Patch"), &env(&d)).await;
        assert!(!err, "{out}");
        assert_eq!(std::fs::read_to_string(d.join("i.txt")).unwrap(), "a\na2\nb\nz\n");
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_2_a_patch_that_does_not_apply_changes_nothing() {
        let d = dir("patch-fail");
        let e = env(&d);
        std::fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let before = || std::fs::read_to_string(d.join("a.txt")).unwrap();

        // Stale context: the lines to change are not there any more. The
        // Add before it is not written either — all or nothing.
        let stale = "*** Begin Patch\n*** Add File: b.txt\n+b\n*** Update File: a.txt\n one\n-TWO\n+2\n*** End Patch";
        let (out, err) = run(APPLY_PATCH, &json!({ "input": stale }), &e).await;
        assert!(err && out.contains("does not apply") && out.contains("not in the file") && out.contains("read it again"), "{out}");
        assert_eq!(before(), "one\ntwo\nthree\n");
        assert!(!d.join("b.txt").exists(), "nothing of a failed patch is written");

        // An @@ context line that is not in the file.
        let (out, err) = run(APPLY_PATCH, &json!({ "input": "*** Begin Patch\n*** Update File: a.txt\n@@ fn nowhere()\n-two\n+2\n*** End Patch" }), &e).await;
        assert!(err && out.contains("`@@ fn nowhere()` is not in the file"), "{out}");
        // Files that are not where the patch says.
        let (out, err) = run(APPLY_PATCH, &json!({ "input": "*** Begin Patch\n*** Update File: missing.txt\n-a\n+b\n*** End Patch" }), &e).await;
        assert!(err && out.contains("does not exist"), "{out}");
        let (out, err) = run(APPLY_PATCH, &json!({ "input": "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch" }), &e).await;
        assert!(err && out.contains("already exists"), "{out}");
        let (out, err) = run(APPLY_PATCH, &json!({ "input": "*** Begin Patch\n*** Delete File: nope.txt\n*** End Patch" }), &e).await;
        assert!(err && out.contains("does not exist"), "{out}");
        // Not a patch at all.
        let (out, err) = run(APPLY_PATCH, &json!({ "input": "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+1\n" }), &e).await;
        assert!(err && out.contains("did not parse") && out.contains(BEGIN), "{out}");
        assert!(run(APPLY_PATCH, &json!({ "patch": "x" }), &e).await.0.contains("invalid input"));
        assert_eq!(before(), "one\ntwo\nthree\n");
        // And no patch at all where edits are not allowed.
        let refused = run(APPLY_PATCH, &json!({ "input": "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch" }), &ToolEnv { permission_mode: PermissionMode::Plan, ..e }).await;
        assert!(refused.1 && refused.0.contains("plan mode") && d.join("a.txt").exists(), "{refused:?}");
        let _ = std::fs::remove_dir_all(d);
    }
}
