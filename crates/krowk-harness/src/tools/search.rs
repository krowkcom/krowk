//! `grep` and `glob`: finding files and lines without a shell.
//!
//! Both walk the files git would show: inside a work tree, the list is
//! `git ls-files --cached --others --exclude-standard`, so `.gitignore` —
//! nested ones, `.git/info/exclude`, the global excludes file — is honoured
//! by git's own rules rather than by a second implementation of them.
//! Outside a work tree there is no `.gitignore` to honour (ripgrep's rule
//! too), and the walk skips only `.git` directories. Symlinked directories
//! are not followed, binary files are never searched, and both the walk and
//! the output are capped, so a search from `/` costs a bounded wait and a
//! bounded answer.

use super::{Scope, open_regular};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Files a walk visits at most.
const WALK_MAX_FILES: usize = 200_000;
/// And how long it runs at most, reading included.
const WALK_DEADLINE: Duration = Duration::from_secs(20);
/// The bytes a file is sniffed for a NUL to call it binary: git's rule.
const SNIFF: usize = 8 << 10;
/// grep skips files bigger than this: generated, minified or data.
const GREP_MAX_FILE: u64 = 16 << 20;
const GREP_MAX_MATCHES: usize = 200;
const GREP_MAX_LINE: usize = 300;
const MAX_OUTPUT: usize = 30_000;
const GLOB_MAX_RESULTS: usize = 1000;

/// Search file contents with a regular expression.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    /// The regular expression, in Rust regex syntax (e.g. `fn\s+main`, `TODO|FIXME`).
    pub pattern: String,
    /// A file or directory to search; the working directory when absent.
    #[serde(default)]
    pub path: Option<String>,
    /// Only search files whose path matches this glob, e.g. `*.rs` or `src/**/*.ts`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Match without regard to case.
    #[serde(default)]
    pub ignore_case: Option<bool>,
}

/// Find files by name.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    /// The glob: `*` and `?` within a path segment, `**` across them, `[abc]` and `{a,b}`. Without a `/` it matches the file name at any depth.
    pub pattern: String,
    /// The directory to search; the working directory when absent.
    #[serde(default)]
    pub path: Option<String>,
}

/// A walk's files, relative to its root, sorted; and whether it stopped
/// short at a cap.
struct Walk {
    files: Vec<PathBuf>,
    truncated: bool,
}

/// The files under `root` git would show, or every file when `root` is
/// not in a work tree.
fn walk(root: &Path, deadline: Instant) -> Walk {
    // A directory git ignores, searched by name, is searched whole: asking
    // for it is the point (ripgrep's rule too).
    let listed = if ignored(root) { None } else { git_files(root) };
    let mut w = listed.unwrap_or_else(|| plain_walk(root, deadline));
    w.files.sort();
    w
}

/// git, run so that neither its fsmonitor (which `ls-files` and
/// `check-ignore` would execute) nor a hook runs with it: a repository is a
/// directory the model may have been handed. No optional locks either: a
/// search never writes the index (`krowk_api::git`).
fn git(root: &Path) -> std::io::Result<std::process::Command> {
    krowk_api::git::query(root)
}

/// Whether git ignores `root` itself, or a directory it is in.
fn ignored(root: &Path) -> bool {
    git(root)
        .and_then(|mut c| {
            c.arg("check-ignore").arg("-q").arg("--").arg(root).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()
        })
        .is_ok_and(|s| s.success())
}

fn git_files(root: &Path) -> Option<Walk> {
    let out = git(root)
        .ok()?
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut files = Vec::new();
    let mut truncated = false;
    for name in out.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let Ok(name) = std::str::from_utf8(name) else { continue };
        let rel = PathBuf::from(name);
        // A tracked file deleted from the work tree, or a submodule, is
        // not a file to search.
        if !std::fs::symlink_metadata(root.join(&rel)).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if files.len() == WALK_MAX_FILES {
            truncated = true;
            break;
        }
        files.push(rel);
    }
    Some(Walk { files, truncated })
}

fn plain_walk(root: &Path, deadline: Instant) -> Walk {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut dirs = vec![PathBuf::new()];
    'walk: while let Some(rel) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&rel)) else { continue };
        for e in entries.flatten() {
            if files.len() == WALK_MAX_FILES || Instant::now() > deadline {
                truncated = true;
                break 'walk;
            }
            // Not followed: a symlinked directory can loop, or lead out.
            let Ok(kind) = e.file_type() else { continue };
            let path = rel.join(e.file_name());
            if kind.is_dir() {
                if e.file_name() != ".git" {
                    dirs.push(path);
                }
            } else if kind.is_file() {
                files.push(path);
            }
        }
    }
    Walk { files, truncated }
}

/// How a path is shown: relative to the working directory when it is under
/// it, as given otherwise.
fn shown(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd).unwrap_or(p).display().to_string()
}

/// A search root that must be a directory.
fn dir_root(scope: &Scope, path: Option<&str>, tool: &str) -> Result<PathBuf, String> {
    let root = match path {
        Some(p) => scope.path(p)?,
        None => scope.cwd.clone(),
    };
    match std::fs::metadata(&root) {
        Ok(m) if m.is_dir() => Ok(root),
        Ok(_) => Err(format!("{} is not a directory, which {tool} searches", root.display())),
        Err(_) => Err(format!("{} does not exist", root.display())),
    }
}

pub(super) fn glob(i: &GlobInput, scope: &Scope) -> (String, bool) {
    let cwd = scope.cwd.as_path();
    let matcher = match Glob::new(&i.pattern) {
        Ok(m) => m,
        Err(e) => return (e, true),
    };
    let root = match dir_root(scope, i.path.as_deref(), "glob") {
        Ok(r) => r,
        Err(e) => return (e, true),
    };
    let deadline = Instant::now() + WALK_DEADLINE;
    let mut w = walk(&root, deadline);
    let mut hits: Vec<&PathBuf> = Vec::new();
    let sandboxed = scope.walk_hides(&root);
    for f in &w.files {
        if Instant::now() > deadline {
            w.truncated = true;
            break;
        }
        if matcher.matches(f) && !scope.hidden.hides(&root.join(f)) && !sandboxed(f) {
            hits.push(f);
        }
    }
    if hits.is_empty() {
        return (format!("no files match {:?} under {}{}", i.pattern, root.display(), if w.truncated { " (the walk stopped at its cap)" } else { "" }), false);
    }
    let mut out = String::new();
    for f in hits.iter().take(GLOB_MAX_RESULTS) {
        out += &shown(cwd, &root.join(f));
        out.push('\n');
    }
    if hits.len() > GLOB_MAX_RESULTS {
        out += &format!("({} files match; these are the first {GLOB_MAX_RESULTS}. Narrow the pattern or the path.)\n", hits.len());
    } else if w.truncated {
        out += &format!("(the walk stopped after {} files, so there may be more)\n", w.files.len());
    }
    (out, false)
}

/// Whether a file's first bytes say binary.
fn binary(head: &[u8]) -> bool {
    head[..head.len().min(SNIFF)].contains(&0)
}

pub(super) fn grep(i: &GrepInput, scope: &Scope) -> (String, bool) {
    let cwd = scope.cwd.as_path();
    let re = match regex_lite::RegexBuilder::new(&i.pattern).case_insensitive(i.ignore_case.unwrap_or(false)).build() {
        Ok(r) => r,
        Err(e) => return (format!("the pattern is not a valid regular expression: {e}"), true),
    };
    let filter = match i.glob.as_deref().map(Glob::new).transpose() {
        Ok(f) => f,
        Err(e) => return (e, true),
    };
    let target = match i.path.as_deref().map(|p| scope.path(p)).transpose() {
        Ok(t) => t.unwrap_or_else(|| cwd.to_path_buf()),
        Err(e) => return (e, true),
    };
    let deadline = Instant::now() + WALK_DEADLINE;
    // One named file is searched whatever git thinks of it, and refused
    // when it is binary; a directory is walked.
    let (root, files, mut truncated) = match std::fs::metadata(&target) {
        Ok(m) if m.is_file() => {
            let (mut f, _) = match open_regular(&target, scope.sandbox.is_some()) {
                Ok(f) => f,
                Err(e) => return (e.replace("which read does not open", "which grep does not search"), true),
            };
            let mut head = vec![0u8; SNIFF];
            let n = std::io::Read::read(&mut f, &mut head).unwrap_or(0);
            if binary(&head[..n]) {
                return (format!("{} is a binary file, which grep does not search", target.display()), true);
            }
            (target.parent().unwrap_or(cwd).to_path_buf(), vec![PathBuf::from(target.file_name().unwrap_or_default())], false)
        }
        Ok(m) if m.is_dir() => {
            let w = walk(&target, deadline);
            (target, w.files, w.truncated)
        }
        Ok(_) => return (format!("{} is not a regular file or a directory, which grep does not search", target.display()), true),
        Err(_) => return (format!("{} does not exist", target.display()), true),
    };
    let mut out = String::new();
    let (mut matches, mut full) = (0usize, false);
    let sandboxed = scope.walk_hides(&root);
    for rel in &files {
        if Instant::now() > deadline {
            truncated = true;
            break;
        }
        if filter.as_ref().is_some_and(|g| !g.matches(rel)) {
            continue;
        }
        let path = root.join(rel);
        if scope.hidden.hides(&path) || sandboxed(rel) {
            continue;
        }
        let Ok((f, size)) = open_regular(&path, scope.sandbox.is_some()) else { continue };
        if size > GREP_MAX_FILE {
            continue;
        }
        let mut raw = Vec::with_capacity(size as usize);
        if std::io::Read::read_to_end(&mut std::io::Read::take(f, GREP_MAX_FILE), &mut raw).is_err() || binary(&raw) {
            continue;
        }
        let text = String::from_utf8_lossy(&raw);
        let name = shown(cwd, &path);
        for (n, line) in text.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            matches += 1;
            if full {
                continue;
            }
            let line = line.trim_end_matches('\r');
            let line = if line.chars().count() > GREP_MAX_LINE { line.chars().take(GREP_MAX_LINE).collect::<String>() + "…" } else { line.to_string() };
            let row = format!("{name}:{}:{line}\n", n + 1);
            if matches > GREP_MAX_MATCHES || out.len() + row.len() > MAX_OUTPUT {
                full = true;
                continue;
            }
            out += &row;
        }
    }
    if matches == 0 {
        return (format!("no matches for {:?}{}", i.pattern, if truncated { " (the search stopped at its cap)" } else { "" }), false);
    }
    if full {
        out += &format!("({matches} matching lines; these are the first. Narrow the pattern, the path or the glob.)\n");
    } else if truncated {
        out += "(the search stopped at its cap, so there may be more)\n";
    }
    (out, false)
}

/// A compiled glob: `*`, `?`, `**`, `[…]` and `{a,b}`, over `/`-separated
/// paths. A pattern with no `/` matches the file name at any depth, as in
/// `.gitignore` and ripgrep.
pub(crate) struct Glob {
    alternatives: Vec<Vec<char>>,
    name_only: bool,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.is_empty() {
            return Err("the glob is empty".into());
        }
        let mut alternatives = Vec::new();
        expand(pattern, &mut alternatives)?;
        Ok(Glob { name_only: !pattern.contains('/'), alternatives: alternatives.into_iter().map(|a| a.chars().collect()).collect() })
    }

    pub fn matches(&self, path: &Path) -> bool {
        let full = path.to_string_lossy().replace('\\', "/");
        let subject: Vec<char> = if self.name_only { full.rsplit('/').next().unwrap_or_default().chars().collect() } else { full.chars().collect() };
        self.alternatives.iter().any(|p| glob_match(p, &subject))
    }
}

/// `{a,b}` expanded into alternatives, nested braces included; at most 256.
fn expand(p: &str, out: &mut Vec<String>) -> Result<(), String> {
    let Some(open) = p.find('{') else {
        out.push(p.to_string());
        return Ok(());
    };
    let (mut depth, mut close, mut commas) = (0, None, vec![]);
    for (i, c) in p[open..].char_indices().map(|(i, c)| (i + open, c)) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let close = close.ok_or_else(|| format!("the glob {p:?} has a `{{` with no `}}`"))?;
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    for w in bounds.windows(2) {
        expand(&format!("{}{}{}", &p[..open], &p[w[0] + 1..w[1]], &p[close + 1..]), out)?;
        if out.len() > 256 {
            return Err("the glob expands to more than 256 alternatives".into());
        }
    }
    Ok(())
}

/// Matches one alternative against a path, segment by segment: `**` as a
/// whole segment spans any number of segments, including none; `*` and
/// `?` stay within one. Both levels backtrack only to the last star, so a
/// pattern costs at most the product of the two lengths — never the
/// exponential a recursive matcher pays on `*a*a*a…b`.
fn glob_match(p: &[char], s: &[char]) -> bool {
    let pats: Vec<&[char]> = p.split(|c| *c == '/').collect();
    let segs: Vec<&[char]> = s.split(|c| *c == '/').collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < segs.len() {
        if pi < pats.len() && pats[pi] == ['*', '*'] {
            star = Some((pi, si));
            pi += 1;
            continue;
        }
        if pi < pats.len() && segment_match(pats[pi], segs[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        match star {
            // The last `**` takes one more segment, and the rest is tried again.
            Some((sp, ss)) => {
                star = Some((sp, ss + 1));
                pi = sp + 1;
                si = ss + 1;
            }
            None => return false,
        }
    }
    pats[pi..].iter().all(|p| *p == ['*', '*'])
}

/// One segment against one pattern segment: `*`, `?`, `[…]` and literals.
fn segment_match(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match class(&p[pi..]) {
                Some((hit, len)) => hit(s[si]).then_some(len),
                None => (s[si] == '[').then_some(1),
            },
            Some(c) => (*c == s[si]).then_some(1),
            None => None,
        };
        match (step, star) {
            (Some(len), _) => {
                pi += len;
                si += 1;
            }
            (None, Some((sp, ss))) => {
                star = Some((sp, ss + 1));
                pi = sp + 1;
                si = ss + 1;
            }
            (None, None) => return false,
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

type ClassFn = Box<dyn Fn(char) -> bool>;

/// A `[…]` class at the front of `p`: its test and its length. None when
/// the bracket never closes, which makes it a literal `[`.
fn class(p: &[char]) -> Option<(ClassFn, usize)> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let first = i;
    while let Some(&c) = p.get(i) {
        if c == ']' && i > first {
            let hit = move |x: char| ranges.iter().any(|&(a, b)| a <= x && x <= b) != negated;
            return Some((Box::new(hit), i + 1));
        }
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
            ranges.push((c, p[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::tests::dir;
    use super::super::*;
    use super::*;
    use serde_json::json;

    fn m(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).unwrap().matches(Path::new(path))
    }

    #[test]
    fn globs_match_like_gitignore_and_ripgrep() {
        assert!(m("*.rs", "main.rs") && m("*.rs", "src/deep/lib.rs"), "no slash: the name at any depth");
        assert!(!m("*.rs", "main.rsx"));
        assert!(m("src/*.rs", "src/lib.rs") && !m("src/*.rs", "src/a/lib.rs"), "* stays in its segment");
        assert!(m("src/**/*.rs", "src/lib.rs") && m("src/**/*.rs", "src/a/b/lib.rs"), "** spans zero or more");
        assert!(m("**/test_*.py", "test_a.py") && m("**/test_*.py", "x/test_a.py"));
        assert!(m("docs/**", "docs/a/b.md") && !m("docs/**", "src/docs.md"));
        assert!(m("*.{ts,tsx}", "a/b.tsx") && m("*.{ts,tsx}", "b.ts") && !m("*.{ts,tsx}", "b.js"));
        assert!(m("file?.txt", "file1.txt") && !m("file?.txt", "file12.txt"));
        assert!(m("[ab]*.md", "b.md") && !m("[!ab]*.md", "a.md") && m("[a-c]x", "cx"));
        assert!(Glob::new("{a,b").is_err() && Glob::new(" ").is_err());
        assert!(m("a/**/b/**/c.txt", "a/x/b/y/z/c.txt") && !m("a/**/b/**/c.txt", "a/x/c.txt"));
        assert!(m("*a*b", "xxaxxb") && !m("*a*b", "xxaxxc") && m("**", "any/depth/at/all"));
    }

    #[test]
    fn a_pathological_glob_costs_linear_time_not_exponential() {
        // `*a` ten times then `*b`, against a long name of `a`s with no b: a
        // recursive matcher takes seconds per file on this.
        let pattern = format!("{}*b", "*a".repeat(10));
        let deep = format!("{}/**/{}", "**/".repeat(10).trim_end_matches('/'), pattern);
        let name = "a".repeat(200);
        let path = format!("{}/{name}", vec!["d"; 50].join("/"));
        let started = Instant::now();
        for _ in 0..100 {
            assert!(!m(&pattern, &name));
            assert!(!m(&deep, &path));
        }
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    fn git_repo(name: &str) -> Option<PathBuf> {
        let d = dir(name);
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&d).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("git is not installed: skipping the .gitignore half");
            return None;
        }
        std::fs::write(d.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(d.join("src/nested")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {\n    // TODO: say hello\n    println!(\"hi\");\n}\n").unwrap();
        std::fs::write(d.join("src/nested/.gitignore"), "gen.rs\n").unwrap();
        std::fs::write(d.join("src/nested/gen.rs"), "// TODO generated\n").unwrap();
        std::fs::write(d.join("src/nested/lib.rs"), "pub fn todo() {}\n").unwrap();
        std::fs::write(d.join("target/out.rs"), "// TODO built\n").unwrap();
        std::fs::write(d.join("debug.log"), "TODO in a log\n").unwrap();
        std::fs::write(d.join("blob.bin"), b"TODO\0binary").unwrap();
        Some(d)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_1_grep_and_glob_respect_gitignore_and_skip_binaries() {
        let Some(d) = git_repo("search") else { return };
        let e = ToolEnv { cwd: &d, permission_mode: PermissionMode::Default, edit: EditTool::StrReplace, evidence: None, builds: None, live: None, env: &[], jobs: None, steers: None };
        let (out, err) = run(GREP, &json!({"pattern": "TODO"}), &e).await;
        assert!(!err, "{out}");
        assert_eq!(out, "src/main.rs:2:    // TODO: say hello\n", "ignored files (root and nested .gitignore) and binaries are not searched");
        let (out, _) = run(GREP, &json!({"pattern": "todo", "ignore_case": true, "glob": "*.rs"}), &e).await;
        assert_eq!(out, "src/main.rs:2:    // TODO: say hello\nsrc/nested/lib.rs:1:pub fn todo() {}\n");
        let (out, _) = run(GREP, &json!({"pattern": "fn", "path": "src/nested"}), &e).await;
        assert_eq!(out, "src/nested/lib.rs:1:pub fn todo() {}\n");
        // A file named outright is searched whatever git thinks of it —
        // unless it is binary.
        assert_eq!(run(GREP, &json!({"pattern": "TODO", "path": "debug.log"}), &e).await, ("debug.log:1:TODO in a log\n".into(), false));
        let (out, err) = run(GREP, &json!({"pattern": "TODO", "path": "blob.bin"}), &e).await;
        assert!(err && out.contains("binary"), "{out}");
        assert_eq!(run(GREP, &json!({"pattern": "nothing-here"}), &e).await, ("no matches for \"nothing-here\"".into(), false));
        let (out, err) = run(GREP, &json!({"pattern": "("}), &e).await;
        assert!(err && out.contains("not a valid regular expression"), "{out}");

        let (out, err) = run(GLOB, &json!({"pattern": "*.rs"}), &e).await;
        assert!(!err);
        assert_eq!(out, "src/main.rs\nsrc/nested/lib.rs\n");
        assert_eq!(run(GLOB, &json!({"pattern": "*", "path": "src/nested"}), &e).await.0, "src/nested/.gitignore\nsrc/nested/lib.rs\n");
        assert!(run(GLOB, &json!({"pattern": "*.log"}), &e).await.0.starts_with("no files match"));
        assert!(run(GLOB, &json!({"pattern": "*", "path": "src/main.rs"}), &e).await.0.contains("not a directory"));
        // A directory git ignores, named outright, is searched whole.
        assert_eq!(run(GREP, &json!({"pattern": "TODO", "path": "target"}), &e).await.0, "target/out.rs:1:// TODO built\n");
        assert_eq!(run(GLOB, &json!({"pattern": "*.rs", "path": "target"}), &e).await.0, "target/out.rs\n");
        // An untracked, unignored file is found as soon as it exists.
        std::fs::write(d.join("src/fresh.rs"), "// TODO fresh\n").unwrap();
        assert!(run(GLOB, &json!({"pattern": "src/*.rs"}), &e).await.0.contains("src/fresh.rs"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_1_grep_output_is_capped_and_outside_git_everything_but_dot_git_is_walked() {
        let d = dir("search-plain");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join(".git/HEAD"), "match\n").unwrap();
        std::fs::write(d.join("many.txt"), "match\n".repeat(500)).unwrap();
        std::fs::write(d.join("wide.txt"), format!("match{}\n", "x".repeat(5000))).unwrap();
        let e = ToolEnv { cwd: &d, permission_mode: PermissionMode::Default, edit: EditTool::StrReplace, evidence: None, builds: None, live: None, env: &[], jobs: None, steers: None };
        // Not a work tree: an empty .git directory is not a repository.
        let (out, err) = run(GREP, &json!({"pattern": "^match"}), &e).await;
        assert!(!err && !out.contains(".git/HEAD"), "{out}");
        assert_eq!(out.lines().filter(|l| l.starts_with("many.txt:")).count(), GREP_MAX_MATCHES);
        assert!(out.contains("501 matching lines") && out.len() <= MAX_OUTPUT + 200, "{}", out.len());
        let (out, _) = run(GREP, &json!({"pattern": "^match", "path": "wide.txt"}), &e).await;
        assert!(out.len() < 400 && out.trim_end().ends_with('…'), "a long line is cut: {}", out.len());
        let _ = std::fs::remove_dir_all(d);
    }
}
