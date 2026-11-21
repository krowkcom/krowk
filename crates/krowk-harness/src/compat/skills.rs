//! Claude-format skills (R-COMPAT-1): a directory holding a `SKILL.md`
//! whose front matter names it and says what it is for. A person asks for
//! one as `/name`, and its body goes to the backend with the prompt
//! (`invoked`): a vendor's agent may not read the directory it is in.
//!
//! Skills are found in krowk's home (`skills/`), `~/.agents/skills`, Claude
//! Code's user directory (`skills/`), and `.agents/skills` then
//! `.claude/skills` in every directory from the repository's root down to the
//! working directory; a skill of the same name found later — deeper, or
//! Claude's beside the shared one — replaces the one before. A `SKILL.md` with
//! no description is not listed: the description is what the model chooses
//! by.

use crate::permissions::Config;
use std::path::{Path, PathBuf};

/// One skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Its directory.
    pub dir: PathBuf,
    /// Whether the person can ask for it as `/name` (front matter
    /// `user-invocable`, true unless it says `false`).
    pub user_invocable: bool,
}

/// A skill's body is capped here: the rest is for the model to read.
const BODY_CAP: usize = 256 << 10;

fn read_dir_of(root: &Path, out: &mut Vec<Skill>, repo: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file()).collect();
    dirs.sort();
    for dir in dirs {
        // A repository's skill that links out of the repository is not
        // taken: its directory becomes readable, and its file is read.
        if let Some(repo) = repo
            && !dir.join("SKILL.md").canonicalize().is_ok_and(|c| c.starts_with(repo))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else { continue };
        let (fm, _) = front_matter(&text);
        let get = |k: &str| fm.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let name = Some(get("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
        let description = get("description").split_whitespace().collect::<Vec<_>>().join(" ");
        if description.is_empty() {
            continue;
        }
        let user_invocable = get("user-invocable").trim() != "false";
        out.retain(|s| s.name != name);
        out.push(Skill { name, description, dir, user_invocable });
    }
}

/// Every skill that applies in `cwd`.
pub fn discover(cfg: &Config, cwd: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    if let Some(d) = &cfg.krowk_dir {
        read_dir_of(&d.join("skills"), &mut out, None);
    }
    if let Some(h) = &cfg.home {
        read_dir_of(&h.join(".agents/skills"), &mut out, None);
    }
    if let Some(d) = cfg.claude_home() {
        read_dir_of(&d.join("skills"), &mut out, None);
    }
    let root = crate::trust::root(cwd);
    for dir in crate::permissions::settings::chain(&root, cwd) {
        read_dir_of(&dir.join(".agents/skills"), &mut out, Some(&root));
        read_dir_of(&dir.join(".claude/skills"), &mut out, Some(&root));
    }
    out
}

/// What a skill the person asked for starts with in the log: clients show
/// it as krowk's, not the person's words.
pub const INVOKED: &str = "<skill name=\"";

/// A prompt that asks for a skill — `/name`, then what it is for — as the
/// model reads it next to the prompt: the skill's instructions, with
/// `$ARGUMENTS` replaced by the words after
/// the name, as Claude Code does. None when the prompt names no skill the
/// person may ask for.
pub fn invoked(list: &[Skill], prompt: &str) -> Option<String> {
    let rest = prompt.strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    list.iter().find(|k| k.name == name && k.user_invocable)?;
    let (body, failed) = load(list, name);
    let body = body.replace("$ARGUMENTS", rest.trim_start()[name.len()..].trim());
    (!failed).then(|| format!("{INVOKED}{name}\">\nThe person asked for this skill with /{name}; follow it for the rest of their message.\n\n{body}\n</skill>"))
}

/// The body of the skill named, and whether reading it failed.
fn load(list: &[Skill], name: &str) -> (String, bool) {
    let Some(k) = list.iter().find(|k| k.name == name.trim()) else {
        let names: Vec<&str> = list.iter().map(|k| k.name.as_str()).collect();
        return (format!("there is no skill named {name:?} — the skills are {}", names.join(", ")), true);
    };
    let text = match std::fs::read_to_string(k.dir.join("SKILL.md")) {
        Ok(t) => t,
        Err(e) => return (format!("the skill {name:?} could not be read: {e}"), true),
    };
    let (_, body) = front_matter(&text);
    let body = if body.len() > BODY_CAP { &body[..body.floor_char_boundary(BODY_CAP)] } else { body };
    (format!("{}\n\n(The skill's files are in {}; read them from there.)", body.trim_end(), k.dir.display()), false)
}

/// Splits `---` front matter from a body. Returns (the front matter's
/// `key: value` pairs, the body).
fn front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t.strip_prefix("---").and_then(|r| r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n"))) else { return (Vec::new(), t) };
    let Some(end) = rest.find("\n---") else { return (Vec::new(), t) };
    let head = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    (pairs(head), body)
}

/// The `key: value` pairs of YAML front matter, as far as skills and rules
/// use it: plain and quoted scalars, and the `>` (folded) and `|` (literal)
/// block scalars a long `description` is often written in, with their
/// `-`/`+` chomping. Nested maps and lists are skipped.
fn pairs(head: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = head.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let block = v.chars().next().filter(|c| *c == '>' || *c == '|') ;
        let value = match block {
            Some(style) if v[1..].chars().all(|c| matches!(c, '-' | '+' | '0'..='9')) => {
                let mut body = Vec::new();
                while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with([' ', '\t'])) {
                    body.push(lines[i].trim());
                    i += 1;
                }
                while body.last().is_some_and(|l| l.is_empty()) {
                    body.pop();
                }
                if style == '|' { body.join("\n") } else { body.join(" ").split_whitespace().collect::<Vec<_>>().join(" ") }
            }
            _ => {
                // A plain scalar may run on over indented lines.
                let mut v = v.trim_matches('"').trim_matches('\'').to_string();
                while i < lines.len() && lines[i].starts_with([' ', '\t']) && !lines[i].trim().is_empty() && !lines[i].trim_start().starts_with('-') {
                    v.push(' ');
                    v.push_str(lines[i].trim());
                    i += 1;
                }
                v
            }
        };
        out.push((k.trim().to_string(), value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_compat_1_skills_are_read_from_agents_and_claude_directories_and_slash_loads_one() {
        let base = std::env::temp_dir().join(format!("krowk-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Canonical, as the repository root is found: macOS's temporary
        // directory is a symlink into /private.
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        let base = base.canonicalize().unwrap();
        let skill = |dir: &str, name: &str, says: &str| {
            let d = base.join(dir).join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), format!("---\nname: {name}\ndescription: {says}\n---\nBODY {says}")).unwrap();
        };
        skill("home/.agents/skills", "implement", "shared by every agent");
        skill("home/.agents/skills", "review", "shared review");
        skill("home/.claude/skills", "review", "claude's review");
        skill("repo/.agents/skills", "deploy", "the repository's");
        let cfg = Config { home: Some(base.join("home")), ..Config::default() };
        let found = discover(&cfg, &base.join("repo"));
        let got: Vec<(&str, &str)> = found.iter().map(|k| (k.name.as_str(), k.description.as_str())).collect();
        assert_eq!(got, [("implement", "shared by every agent"), ("review", "claude's review"), ("deploy", "the repository's")], "Claude's beside the shared one wins");

        let text = invoked(&found, "/implement the login page").expect("a skill only in .agents is loaded by /name");
        assert!(text.starts_with(&format!("{INVOKED}implement\">")) && text.contains("BODY shared by every agent"), "{text}");
        assert_eq!(invoked(&found, "/nope do it"), None, "a name no skill has is left to the prompt");

        std::fs::write(base.join("repo/.agents/skills/deploy/SKILL.md"), "---\nname: deploy\ndescription: ship\n---\nDeploy $ARGUMENTS now.").unwrap();
        let found = discover(&cfg, &base.join("repo"));
        assert!(invoked(&found, "/deploy  staging eu ").unwrap().contains("Deploy staging eu now."), "$ARGUMENTS is the words after the name");
        assert!(invoked(&found, "/deploy").unwrap().contains("Deploy  now."), "and empty without any");
        // A description in a YAML block scalar, as long ones are written.
        let (fm, body) = front_matter("---\nname: notes\ndescription: >-\n  Write the release\n  notes for a tag\nother: |\n  a\n  b\n---\nbody");
        assert_eq!(fm, [("name".into(), "notes".into()), ("description".into(), "Write the release notes for a tag".into()), ("other".into(), "a\nb".into())]);
        assert_eq!(body, "body");
        let _ = std::fs::remove_dir_all(&base);
    }
}
