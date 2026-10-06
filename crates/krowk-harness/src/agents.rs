//! Agent definitions (R-SUB-5): what a subagent is told, which model it
//! runs on and which tools it may use, written as Markdown with a
//! frontmatter — krowk's own format, and Claude Code's, read as it is.
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews a diff for correctness bugs. Use after a change.
//! model: inherit
//! tools: read, grep, glob
//! ---
//! You review diffs. Read every changed file whole before judging it…
//! ```
//!
//! - `name` — what the `subagent` tool's `agent` names it by; the file's
//!   stem when absent.
//! - `description` — when to use it: the parent's model reads it in the
//!   tool's description, so it decides when the agent is called.
//! - `model` — `inherit` (the parent's), an alias of Claude Code's
//!   (`haiku`, `sonnet`, `opus`), `<instance>/<model>` or a bare model id;
//!   absent, the subagent default (the config's, else the catalog's cheaper
//!   tier below the parent's model).
//! - `tools` — the allowlist, as a comma-separated string or a list: krowk's
//!   names (`read`, `write`, `edit`, `bash`, `grep`, `glob`, `todo_write`,
//!   `publish`) or Claude Code's (`Read`, `Write`, `Edit`, `MultiEdit`,
//!   `Bash`, `Grep`, `Glob`, `TodoWrite`); absent, every tool. A name krowk
//!   has no tool for — `WebFetch`, an MCP tool — is left out, and so is the
//!   subagent tool itself: subagents do not start subagents.
//! - `isolation` — `worktree` runs it in a git worktree of its own
//!   (`crate::worktree`), as Claude Code's does; `none`, or absent, in the
//!   parent's directory. A call's own `isolation` wins.
//! - The body is the subagent's instructions, after krowk's own system
//!   prompt.
//!
//! Where they are read from, first found wins by name: the repository's
//! `.krowk/agents/`, its `.claude/agents/`, then the person's (krowk's config
//! directory's `agents/`, Claude Code's `~/.claude/agents/`). A
//! repository's definition can only narrow a subagent: the permission mode
//! is always the parent's, so an allowlist never grants what the mode
//! refuses, and nothing in a definition is run.

use crate::subagent::Isolation;
use std::path::{Path, PathBuf};

/// One definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// As written; resolved against the parent's model when it runs.
    pub model: Option<String>,
    /// krowk's tool names, the edit tool as `edit`; none is every tool.
    pub tools: Option<Vec<String>>,
    /// Where it works, when the definition says.
    pub isolation: Option<Isolation>,
    /// The body: what the subagent is told beyond krowk's own prompt.
    pub instructions: String,
    pub path: PathBuf,
    /// Read from the repository rather than the person's own directories:
    /// someone else may have written it, so until the repository is trusted
    /// its `model` resolves only on the parent's own instance.
    pub project: bool,
}

/// The directories a repository holds definitions in, krowk's first.
pub const PROJECT_DIRS: [&str; 2] = [".krowk/agents", ".claude/agents"];

/// Every definition for a session in `root` (its repository), then those
/// in `user_dirs`, first by name winning — names compared regardless of
/// case, so a repository's `Reviewer` replaces the person's `reviewer`, as
/// Claude Code's project agents replace its user agents. That is the name
/// a permission rule judges, matched regardless of case too, so replacing a
/// definition never dodges a `Task(reviewer)` rule. A file that does not parse is
/// skipped with the reason, for the caller to say.
pub fn discover(root: &Path, user_dirs: &[PathBuf]) -> (Vec<AgentDef>, Vec<String>) {
    let mut defs: Vec<AgentDef> = Vec::new();
    let mut problems = Vec::new();
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let dirs = PROJECT_DIRS.iter().map(|d| (root.join(d), Some(*d))).chain(user_dirs.iter().map(|d| (d.clone(), None)));
    for (dir, rel) in dirs {
        let project = rel.is_some();
        // A repository's definitions are its own files: a symlink out of it
        // — the directory, a parent of it, or a file in it — could make any
        // file on the disk one, so none is followed.
        if let Some(rel) = rel
            && (is_link_on_the_way(root, rel) || !dir.canonicalize().is_ok_and(|d| d.starts_with(&real_root)))
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|e| !project || e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file())
            .collect();
        files.sort();
        for path in files {
            // A definition is a page of Markdown; a huge file is not one.
            let text = match std::fs::metadata(&path) {
                Ok(m) if m.len() <= 256 << 10 => std::fs::read_to_string(&path).unwrap_or_default(),
                _ => continue,
            };
            match parse(&text, &path) {
                // One definition a name, whatever its case — the name is
                // looked up regardless of case — the first found winning.
                Ok(d) if !defs.iter().any(|e| e.name.eq_ignore_ascii_case(&d.name)) => defs.push(AgentDef { project, ..d }),
                Ok(_) => {}
                Err(why) => problems.push(format!("{}: {why}", path.display())),
            }
        }
    }
    (defs, problems)
}

/// Whether any part of `rel` under `root` (`.claude`, then `.claude/agents`)
/// is a symlink.
fn is_link_on_the_way(root: &Path, rel: &str) -> bool {
    let mut at = root.to_path_buf();
    rel.split('/').any(|part| {
        at.push(part);
        at.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
    })
}

/// One definition from its file's text.
pub fn parse(text: &str, path: &Path) -> Result<AgentDef, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(|l| l.trim_end()) != Some("---") {
        return Err("no frontmatter: the file must start with a `---` line".into());
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut closed = false;
    let mut consumed = text.find('\n').map_or(text.len(), |i| i + 1);
    for raw in lines.by_ref() {
        consumed += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        // A YAML list item continues the key before it.
        if let Some(item) = line.trim_start().strip_prefix("- ")
            && line.starts_with([' ', '\t', '-'])
            && let Some((_, v)) = fields.last_mut()
        {
            if !v.is_empty() {
                v.push(',');
            }
            v.push_str(unquote(item.trim()));
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.starts_with([' ', '\t', '#']) {
            continue;
        }
        fields.push((k.trim().to_string(), unquote(v.trim()).to_string()));
    }
    if !closed {
        return Err("the frontmatter is not closed by a `---` line".into());
    }
    let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
    let name = get("name").unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("{name:?} is not a name: letters, digits, `-`, `_` and `.` only"));
    }
    let tools = get("tools").map(|t| {
        let mut out: Vec<String> = Vec::new();
        for raw in t.trim_start_matches('[').trim_end_matches(']').split(',') {
            if let Some(n) = tool_name(unquote(raw.trim()))
                && !out.iter().any(|o| o == n)
            {
                out.push(n.to_string());
            }
        }
        out
    });
    let isolation = match get("isolation").map(|i| i.to_ascii_lowercase()).as_deref() {
        None => None,
        Some("none") => Some(Isolation::None),
        Some("worktree") => Some(Isolation::Worktree),
        Some(other) => return Err(format!("isolation {other:?} is not one krowk knows: `worktree` or `none`")),
    };
    Ok(AgentDef {
        name,
        description: get("description").unwrap_or_default(),
        model: get("model"),
        tools,
        isolation,
        instructions: text.get(consumed.min(text.len())..).unwrap_or_default().trim().to_string(),
        path: path.to_path_buf(),
        project: false,
    })
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// A tool name, krowk's or Claude Code's, as krowk's; none for one krowk
/// has no tool for, or the subagent tool itself.
fn tool_name(n: &str) -> Option<&'static str> {
    Some(match n.to_ascii_lowercase().as_str() {
        "read" => "read",
        "write" => "write",
        "edit" | "multiedit" | "str_replace" | "apply_patch" | "search_replace" => "edit",
        "bash" => "bash",
        "grep" => "grep",
        "glob" => "glob",
        "todo_write" | "todowrite" => "todo_write",
        "publish" => "publish",
        "skill" => "skill",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_sub_5_a_claude_code_agent_definition_is_read_as_it_is() {
        let claude = "---\nname: code-reviewer\ndescription: \"Reviews code. Use PROACTIVELY after edits.\"\ntools: Read, Grep, Glob, Bash, WebFetch, Task, mcp__github__get_pr\nmodel: haiku\ncolor: purple\n---\n\nYou are a senior reviewer.\n\nBe terse.\n";
        let d = parse(claude, Path::new("/r/.claude/agents/code-reviewer.md")).unwrap();
        assert_eq!(d.name, "code-reviewer");
        assert_eq!(d.description, "Reviews code. Use PROACTIVELY after edits.");
        assert_eq!(d.model.as_deref(), Some("haiku"));
        assert_eq!(d.tools.as_deref(), Some(&["read".to_string(), "grep".into(), "glob".into(), "bash".into()][..]), "what krowk has no tool for is left out, and so is Task");
        assert_eq!(d.instructions, "You are a senior reviewer.\n\nBe terse.");
        assert_eq!(d.isolation, None);
        let isolated = parse("---\nname: editor\nisolation: worktree\n---\nEdit.", Path::new("a.md")).unwrap();
        assert_eq!(isolated.isolation, Some(Isolation::Worktree), "Claude Code's `isolation: worktree`");
        assert!(parse("---\nname: x\nisolation: container\n---\n", Path::new("a.md")).unwrap_err().contains("isolation \"container\""));
        // krowk's own format: a YAML list, and no model or name.
        let native = "---\ndescription: Finds files\ntools:\n  - read\n  - glob\n  - Edit\n---\nFind things.";
        let d = parse(native, Path::new("/r/.krowk/agents/finder.md")).unwrap();
        assert_eq!((d.name.as_str(), d.model, d.tools.as_deref()), ("finder", None, Some(&["read".to_string(), "glob".into(), "edit".into()][..])));
        // No tools line is every tool.
        assert_eq!(parse("---\nname: all\n---\nx", Path::new("a.md")).unwrap().tools, None);
        assert!(parse("no frontmatter", Path::new("a.md")).unwrap_err().contains("frontmatter"));
        assert!(parse("---\nname: x\n", Path::new("a.md")).unwrap_err().contains("not closed"));
        assert!(parse("---\nname: ../evil\n---\n", Path::new("a.md")).unwrap_err().contains("not a name"));
    }

    #[test]
    fn r_sub_5_definitions_are_found_in_the_repository_then_the_persons_first_name_winning() {
        let root = std::env::temp_dir().join(format!("krowk-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let user = root.join("user-agents");
        for d in [repo.join(".krowk/agents"), repo.join(".claude/agents"), user.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(repo.join(".krowk/agents/reviewer.md"), "---\nname: reviewer\ndescription: krowk's\n---\n").unwrap();
        std::fs::write(repo.join(".claude/agents/reviewer.md"), "---\nname: reviewer\ndescription: claude's\n---\n").unwrap();
        std::fs::write(repo.join(".claude/agents/tester.md"), "---\nname: tester\ndescription: runs tests\n---\n").unwrap();
        std::fs::write(repo.join(".claude/agents/broken.md"), "no frontmatter").unwrap();
        std::fs::write(user.join("tester.md"), "---\nname: tester\ndescription: the person's\n---\n").unwrap();
        std::fs::write(user.join("writer.md"), "---\nname: writer\ndescription: writes\n---\n").unwrap();
        std::fs::write(user.join("Reviewer.md"), "---\nname: Reviewer\ndescription: the person's, in another case\n---\n").unwrap();
        std::fs::write(user.join("notes.txt"), "not a definition").unwrap();
        let (defs, problems) = discover(&repo, &[user, root.join("missing")]);
        let got: Vec<(&str, &str)> = defs.iter().map(|d| (d.name.as_str(), d.description.as_str())).collect();
        assert_eq!(got, [("reviewer", "krowk's"), ("tester", "runs tests"), ("writer", "writes")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("broken.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn r_sub_5_a_repositorys_definitions_are_never_read_through_a_symlink_out_of_it() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("krowk-agents-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let outside = root.join("outside");
        std::fs::create_dir_all(repo.join(".krowk/agents")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), "---\nname: secret\n---\nfrom outside").unwrap();
        std::fs::write(repo.join(".krowk/agents/own.md"), "---\nname: own\n---\n").unwrap();
        // A file linked out, and a whole directory linked out.
        symlink(outside.join("secret.md"), repo.join(".krowk/agents/linked.md")).unwrap();
        symlink(&outside, repo.join(".claude")).unwrap();
        let (defs, _) = discover(&repo, &[]);
        assert_eq!(defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["own"]);
        assert!(defs[0].project);
        // The person's own directories are theirs: a link there is followed.
        let (defs, _) = discover(&root.join("nowhere"), &[repo.join(".claude")]);
        assert_eq!((defs.len(), defs[0].project), (1, false));
        let _ = std::fs::remove_dir_all(&root);
    }
}
