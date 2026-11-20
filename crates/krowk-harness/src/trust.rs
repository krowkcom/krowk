//! Which repositories a backend may run in (R-BACK-6). A vendor's agent
//! runs what a repository configures it to run — Claude Code its hooks
//! (`.claude/settings.json`) and MCP servers (`.mcp.json`) without its trust
//! dialog under `-p`; Codex what its project config layers (a `.codex/`
//! anywhere from the working directory up to the repository root) name — so
//! a repository someone else wrote can run code the moment a backend starts
//! in it. Before a backend is spawned, krowk asks its own question, and
//! remembers the answer here.
//!
//! The unit of trust is the repository: the nearest ancestor of the working
//! directory holding a `.git`, else the working directory itself. Trust is
//! that exact directory: a repository cloned inside a trusted one — a
//! vendored checkout, a test fixture — is its own and is asked about on its
//! own, since its hooks are someone else's. The home directory and `/` are
//! never recorded: trusting either would trust every directory without a
//! `.git` of its own under it (a home kept in git for its dotfiles is the
//! usual way to get there). The list lives in krowk's home as
//! `trusted.json`, `0600`, replaced by rename; it is a host's own record and
//! never syncs. A record carries a fingerprint of every file of the
//! repository that can run code (`FINGERPRINTED`): one added or changed
//! since — a `git pull` — puts the repository back to untrusted, and the
//! question names the file. Local-only files (`.claude/settings.local.json`,
//! which is not in git) are not fingerprinted. The native engine consults it for what a repository would
//! widen (`crate::permissions::settings`) and the MCP servers its
//! `.mcp.json` would start (`crate::mcp`).

use crate::engine::EngineError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const FILE: &str = "trusted.json";

/// Asked before a backend is spawned in a repository, with the
/// repository's root: `Ok` to go ahead, or the refusal to report. The
/// client decides how — a list, a flag, a prompt — so the host stays free
/// of terminals.
pub type Gate = Arc<dyn Fn(&Path) -> Result<(), EngineError> + Send + Sync>;

/// A gate for a host that never runs a backend, or a test that trusts
/// everything it made.
pub fn allow_all() -> Gate {
    Arc::new(|_| Ok(()))
}

/// The repository a working directory belongs to, canonical: what trust is
/// granted to.
pub fn root(cwd: &Path) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    cwd.ancestors().find(|d| d.join(".git").symlink_metadata().is_ok()).map(Path::to_path_buf).unwrap_or(cwd)
}

/// The paths of a repository that can run code as the person, fingerprinted
/// when it is trusted: what Claude Code reads from a project (settings and
/// their hooks, hook scripts kept under `.claude/hooks`, commands, agents,
/// skills), what Codex layers from `.codex` (config, rules, hooks), and
/// krowk's own agent definitions and `.krowk/config.json` (hooks, allow rules
/// and the worktree setup command). A directory is every file under it. The
/// project settings, hooks, `.codex` and `.krowk/config.json` of every
/// directory below the root count too (`roots_of_prints`). Not covered: a
/// script a hook or server names from elsewhere in the repository.
/// `.mcp.json` is covered by its own servers digest (`Listed::mcp`).
/// `.claude/settings.local.json` is left out: it is not in git, so a pull
/// cannot change it, and the person edits it themselves.
pub const FINGERPRINTED: &[&str] = &[".claude/settings.json", ".claude/hooks", ".claude/commands", ".claude/agents", ".claude/skills", ".krowk/agents", ".krowk/config.json", ".codex"];

/// What a backend would run of the repository's own, found in it: the
/// reason the question is asked, shown with it.
pub fn what_runs(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for (file, what) in [
        (".claude/settings.json", "its hooks and settings"),
        (".claude/settings.local.json", "its local hooks and settings"),
        (".mcp.json", "its MCP servers"),
        (".claude/hooks", "its hook scripts"),
        (".claude/commands", "its commands"),
        (".claude/agents", "its agents"),
        (".claude/skills", "its skills"),
        (".krowk/agents", "krowk agent definitions"),
        (".codex", "Codex's project config, rules and hooks"),
    ] {
        if root.join(file).symlink_metadata().is_ok() {
            found.push(format!("{file} ({what})"));
        }
    }
    found
}

/// The refusal a gate reports for a repository nobody trusted.
pub fn untrusted(root: &Path, how: &str) -> EngineError {
    let runs = what_runs(root);
    let found = if runs.is_empty() { String::new() } else { format!(" — it has {}", runs.join(", ")) };
    EngineError::new(
        "untrusted_directory",
        format!(
            "{} is not a repository you have trusted, and a backend (Claude Code, Codex) runs what a repository configures it to — hooks, MCP servers — once it starts there{found}. {how}",
            root.display()
        ),
    )
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Listed {
    #[serde(default)]
    directories: Vec<String>,
    /// For each trusted repository whose `.mcp.json` named servers when it
    /// was trusted, a digest of those servers. Trust covers the servers the
    /// person was shown: one added or changed since — a `git pull` — puts
    /// the repository back to untrusted until it is asked about again.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    mcp: std::collections::BTreeMap<String, String>,
    /// For each trusted repository, a digest of every fingerprinted file
    /// that was there when it was trusted (`FINGERPRINTED`), by path from
    /// the root. A repository listed without an entry here was trusted
    /// before files were fingerprinted: `Store::changes` holds it to the
    /// files' ages instead, and the next write upgrades it.
    #[serde(default)]
    files: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// What `Store::changes` reports for a file a legacy record cannot vouch
/// for, and what such a file is recorded as: a digest nothing matches.
const STALE: &str = "changed-before-upgrade";

const MAX_FILES: usize = 4000;
const MAX_FILE: u64 = 32 << 20;
const MAX_DEPTH: usize = 12;
const MAX_DIRS: usize = 20000;
const MAX_BYTES: u64 = 256 << 20;
const MAX_VISITS: usize = 20000;
const MAX_DEPTH_DIRS: usize = 6;

type Prints = std::collections::BTreeMap<String, (String, Option<std::time::SystemTime>)>;

/// What `fingerprints` records when a limit cut it short: a different digest
/// every time, so a repository too big to fingerprint is never remembered as
/// unchanged.
const TRUNCATED: &str = "(too many files to fingerprint)";

/// Folders that hold dependencies or caches, too big to look through for
/// nested settings. One is skipped only while git tracks nothing in it: a
/// committed `venv/.claude/settings.json` is the repository's like any other,
/// and a session started in that folder would read it.
const DEPENDENCY_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox", ".next", ".gradle", ".terraform", ".cache"];

/// Whether git tracks a file under `sub` of `root`. Run without the
/// repository's hooks or fsmonitor (`krowk_api::git::query`), since the
/// repository is not trusted yet. Without git, nothing is tracked.
fn tracked_under(root: &Path, sub: &str) -> bool {
    krowk_api::git::query(root)
        .and_then(|mut c| c.args(["ls-files", "-z", "--cached", "--", sub]).stderr(std::process::Stdio::null()).output())
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// The paths to fingerprint, from the root: `FINGERPRINTED`, and the same
/// project files of every directory below it that a session could start in
/// (a vendor reads `.claude/settings.json` and Codex a `.codex` from the
/// working directory up to the root). A directory with a `.git` of its own is
/// a repository asked about on its own. Bounded in directories and depth.
fn roots_of_prints(root: &Path) -> (Vec<String>, bool) {
    let mut paths: Vec<String> = FINGERPRINTED.iter().map(|p| p.to_string()).collect();
    let (mut seen, mut cut) = (0, false);
    // Shallow first and in name order, so the directories looked into are
    // the same every time and a limit leaves out the deepest, not a branch.
    let mut queue = std::collections::VecDeque::from([(String::new(), 0usize)]);
    while let Some((rel, depth)) = queue.pop_front() {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for name in names {
            if matches!(name.as_str(), ".git" | ".claude" | ".codex" | ".krowk") {
                continue;
            }
            let sub = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if DEPENDENCY_DIRS.contains(&name.as_str()) && !tracked_under(root, &sub) {
                continue;
            }
            if root.join(&sub).join(".git").symlink_metadata().is_ok() {
                continue;
            }
            // Deeper than a session would start is left alone, and is not a
            // change; a tree wider than the limit is, since which of its
            // directories hold a settings file is then unknown.
            if depth >= MAX_DEPTH_DIRS {
                continue;
            }
            seen += 1;
            if seen > MAX_DIRS {
                cut = true;
                continue;
            }
            for f in [".claude/settings.json", ".claude/hooks", ".codex", ".krowk/config.json"] {
                paths.push(format!("{sub}/{f}"));
            }
            queue.push_back((sub, depth + 1));
        }
    }
    (paths, cut)
}

/// Each fingerprinted file of `root` with its digest and modification time:
/// regular files, and symlinks, which are followed when they point inside
/// the repository (their target's contents count) and are otherwise hashed
/// by their target's text, so a FIFO or a link out reads nothing. Bounded
/// in count, depth and size (every path looked at, links included); a limit hit is recorded as a change, and bytes read are bounded overall.
fn fingerprints(root: &Path) -> Prints {
    use sha2::Digest;
    fn walk(root: &Path, canon_root: &Path, rel: &str, depth: usize, budget: &mut (usize, u64), out: &mut Prints) {
        if out.len() >= MAX_FILES || budget.0 > MAX_VISITS {
            out.insert(TRUNCATED.into(), (nonce(), None));
            return;
        }
        let path = root.join(rel);
        let Ok(mut meta) = path.symlink_metadata() else { return };
        // Every path that exists counts, so a directory link back up the
        // tree ends in the limit rather than in a walk of every route.
        budget.0 += 1;
        let mut link = None;
        if meta.file_type().is_symlink() {
            let inside = path.canonicalize().ok().filter(|c| c.starts_with(canon_root));
            match inside.and_then(|_| path.metadata().ok()) {
                Some(m) => meta = m,
                None => link = Some(std::fs::read_link(&path).map(|t| t.display().to_string()).unwrap_or_default()),
            }
        }
        let digest = if let Some(target) = link {
            sha2::Sha256::digest(format!("link:{target}").as_bytes())
        } else if meta.is_dir() {
            if depth >= MAX_DEPTH {
                out.insert(TRUNCATED.into(), (nonce(), None));
            } else if let Ok(entries) = std::fs::read_dir(&path) {
                let mut names: Vec<_> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                names.sort();
                for n in names {
                    walk(root, canon_root, &format!("{rel}/{n}"), depth + 1, budget, out);
                }
            }
            return;
        } else if meta.is_file() {
            let mut h = sha2::Sha256::new();
            // Streamed, so a large file costs no memory, and read no further
            // than the file or the whole walk may be: past either, it is
            // never taken as unchanged.
            let allowed = MAX_FILE.min(MAX_BYTES.saturating_sub(budget.1));
            if meta.len() > allowed {
                out.insert(TRUNCATED.into(), (nonce(), None));
            }
            budget.1 += meta.len().min(allowed);
            let mut f = std::fs::File::open(&path).ok().map(|f| std::io::BufReader::new(std::io::Read::take(f, allowed)));
            let mut buf = [0u8; 64 * 1024];
            while let Some(r) = f.as_mut() {
                match std::io::Read::read(r, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => h.update(&buf[..n]),
                }
            }
            #[cfg(unix)]
            h.update([std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) as u8 & 0o111]);
            h.finalize()
        } else {
            return;
        };
        out.insert(rel.to_string(), (digest.iter().map(|b| format!("{b:02x}")).collect(), meta.modified().ok()));
    }
    fn nonce() -> String {
        format!("{:?}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default())
    }
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Prints::new();
    let mut budget = (0, 0);
    let (paths, cut) = roots_of_prints(root);
    for p in &paths {
        walk(root, &canon_root, p, 0, &mut budget, &mut out);
    }
    if cut {
        out.insert(TRUNCATED.into(), (nonce(), None));
    }
    out
}

/// Why `root` may not be remembered as trusted, if it may not: it is `/`,
/// or the home directory.
pub fn unrecordable(root: &Path, home: Option<&Path>) -> Option<String> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let root = canon(root);
    if root.parent().is_none() {
        return Some("/ is every directory on the machine".into());
    }
    if home.filter(|h| !h.as_os_str().is_empty()).is_some_and(|h| canon(h) == root) {
        return Some(format!("{} is your home directory, and trusting it would trust every directory under it that is not a repository of its own", root.display()));
    }
    None
}

/// The trusted-directories file.
#[derive(Debug, Clone)]
pub struct Store {
    /// None without a home: nothing is trusted, and nothing remembered.
    path: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Store {
    /// The file, and the home directory it will never record.
    pub fn new(path: Option<PathBuf>, home: Option<PathBuf>) -> Store {
        Store { path, home }
    }

    fn read(&self) -> Result<Listed, String> {
        let Some(path) = &self.path else { return Ok(Listed::default()) };
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Listed::default()),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// Whether `root` itself was trusted — not a directory above it. A file
    /// that cannot be read trusts nothing, and neither does an entry for
    /// the home directory or `/`, however it got there.
    pub fn trusts(&self, root: &Path) -> bool {
        let Ok(listed) = self.read() else { return false };
        unrecordable(root, self.home.as_deref()).is_none()
            && listed.directories.iter().any(|d| Path::new(d) == root)
            && self.changes_in(&listed, root).is_empty()
    }

    /// The listed repositories `trusts` still holds for, in the list's
    /// order: what the host daemon announces it will start a session in
    /// (R-HOST-3). Never the home directory or `/`; none when the file does
    /// not read.
    pub fn directories(&self) -> Vec<PathBuf> {
        // One read of the file for all of them, as `trusts` reads it.
        let Ok(listed) = self.read() else { return Vec::new() };
        listed
            .directories
            .iter()
            .map(PathBuf::from)
            .filter(|d| unrecordable(d, self.home.as_deref()).is_none() && self.changes_in(&listed, d).is_empty())
            .collect()
    }

    /// The files of `root` that can run code and are not as they were when it
    /// was trusted — added or changed since, by path from the root, `.mcp.json`
    /// for its servers: why a trusted repository is asked about again. Empty
    /// when `root` is not listed (nothing was trusted to change) or nothing
    /// changed. Local-only files are never among them.
    pub fn changes(&self, root: &Path) -> Vec<String> {
        let Ok(listed) = self.read() else { return Vec::new() };
        if !listed.directories.iter().any(|d| Path::new(d) == root) {
            return Vec::new();
        }
        self.changes_in(&listed, root)
    }

    fn changes_in(&self, listed: &Listed, root: &Path) -> Vec<String> {
        let key = root.display().to_string();
        let mut changed = Vec::new();
        if listed.mcp.get(&key).cloned() != crate::mcp::project_digest(root) {
            changed.push(".mcp.json".to_string());
        }
        let now = fingerprints(root);
        match listed.files.get(&key) {
            Some(was) => changed.extend(now.into_iter().filter(|(p, (d, _))| was.get(p) != Some(d)).map(|(p, _)| p)),
            None => {
                // Trusted before files were fingerprinted: a file counts as
                // unchanged when it is older than the record, which is
                // as old as the list was last written.
                let since = self.path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
                changed.extend(now.into_iter().filter(|(_, (_, m))| !matches!((m, since), (Some(m), Some(s)) if *m < s)).map(|(p, _)| p));
            }
        }
        changed
    }

    /// The refusal for a repository nobody trusted, naming the files that
    /// changed since it was, if it was.
    pub fn untrusted(&self, root: &Path, how: &str) -> EngineError {
        let changed = self.changes(root);
        if changed.is_empty() {
            return untrusted(root, how);
        }
        untrusted(root, &format!("It was trusted before, but {} changed since. {how}", changed.join(", ")))
    }

    /// What the list says of `root`'s MCP servers: `None` when `root` is not
    /// listed at all (trusted, if at all, by `--trust` or for this run), else
    /// the digest its `.mcp.json` servers were trusted with, if any.
    pub fn recorded_mcp(&self, root: &Path) -> Option<Option<String>> {
        let listed = self.read().ok()?;
        let key = root.display().to_string();
        listed.directories.contains(&key).then(|| listed.mcp.get(&key).cloned())
    }

    /// Why `root` cannot be remembered, if it cannot.
    pub fn refuses(&self, root: &Path) -> Option<String> {
        unrecordable(root, self.home.as_deref())
    }

    /// Gives every listed repository but `except` that predates fingerprints
    /// the ones it holds for: a file older than the list stays as it is
    /// now, any other is recorded as changed. Done before the list is
    /// written, since writing moves the age the others are held to. True when
    /// it changed anything.
    fn upgrade(&self, listed: &mut Listed, except: &str) -> bool {
        let legacy: Vec<String> = listed.directories.iter().filter(|d| d.as_str() != except && !listed.files.contains_key(*d)).cloned().collect();
        let mut any = false;
        for d in legacy {
            let was = self.changes_in(listed, Path::new(&d));
            let held: std::collections::BTreeMap<String, String> = fingerprints(Path::new(&d)).into_iter().map(|(p, (dg, _))| { let dg = if was.contains(&p) { STALE.to_string() } else { dg }; (p, dg) }).collect();
            listed.files.insert(d, held);
            any = true;
        }
        any
    }

    /// Remembers `root` as trusted.
    pub fn trust(&self, root: &Path) -> Result<(), String> {
        if let Some(why) = self.refuses(root) {
            return Err(format!("{} is not remembered as trusted: {why}", root.display()));
        }
        let mut listed = self.read()?;
        let digest = crate::mcp::project_digest(root);
        let current: std::collections::BTreeMap<String, String> = fingerprints(root).into_iter().map(|(p, (d, _))| (p, d)).collect();
        let key = root.display().to_string();
        let others = self.upgrade(&mut listed, &key);
        let root = key;
        if !others && listed.directories.contains(&root) && listed.mcp.get(&root) == digest.as_ref() && listed.files.get(&root) == Some(&current) {
            return Ok(());
        }
        listed.files.insert(root.clone(), current);
        match digest {
            Some(d) => listed.mcp.insert(root.clone(), d),
            None => listed.mcp.remove(&root),
        };
        if !listed.directories.contains(&root) {
            listed.directories.push(root);
            listed.directories.sort();
        }
        let path = self.path.as_ref().ok_or("there is no home directory to remember it in — set HOME, or KROWK_HOME to an absolute path")?;
        let dir = path.parent().ok_or_else(|| format!("{} has no directory", path.display()))?;
        crate::log::private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let body = serde_json::to_vec_pretty(&listed).expect("the list serializes");
        std::io::Write::write_all(&mut o.open(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?, &body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_back_6_trust_is_per_repository_covers_what_is_under_it_and_is_remembered() {
        let base = std::env::temp_dir().join(format!("krowk-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src/deep")).unwrap();
        std::fs::create_dir_all(base.join("repo/.claude")).unwrap();
        std::fs::create_dir_all(base.join("repo/.codex")).unwrap();
        std::fs::write(base.join("repo/.mcp.json"), "{}").unwrap();
        std::fs::write(base.join("repo/.claude/settings.json"), "{}").unwrap();
        let repo = base.join("repo").canonicalize().unwrap();
        assert_eq!(root(&base.join("repo/src/deep")), repo, "the repository, not the subdirectory");
        let runs = what_runs(&repo);
        assert!(runs.iter().any(|r| r.starts_with(".mcp.json")) && runs.iter().any(|r| r.starts_with(".claude/settings.json")) && runs.iter().any(|r| r.starts_with(".codex")), "{runs:?}");
        let e = untrusted(&repo, "Pass --trust.");
        assert_eq!(e.code, "untrusted_directory");
        assert!(e.message.contains("hooks, MCP servers") && e.message.contains(".mcp.json") && e.message.ends_with("Pass --trust."), "{}", e.message);

        let home = base.join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let home = home.canonicalize().unwrap();
        let store = Store::new(Some(base.join("config/trusted.json")), Some(home.clone()));
        assert!(!store.trusts(&repo));
        store.trust(&repo).unwrap();
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo) && store.trusts(&root(&repo.join("src"))), "the repository, from anywhere in it");
        assert!(!store.trusts(&repo.join("src")) && !store.trusts(&base), "exactly that directory: not one under it spelled as a root, nor one above");
        // A repository inside a trusted one is its own.
        std::fs::create_dir_all(base.join("repo/vendor/lib/.git")).unwrap();
        let nested = root(&base.join("repo/vendor/lib"));
        assert_eq!(nested, repo.join("vendor/lib"));
        assert!(!store.trusts(&nested), "a nested repository is asked about on its own");
        // A home kept in git for its dotfiles: every plain directory in it
        // resolves to the home, which is never recorded, nor is /.
        assert_eq!(root(&home.join("notes")), home);
        assert!(store.trust(&home).unwrap_err().contains("home directory"));
        assert!(store.trust(Path::new("/")).is_err() && store.refuses(Path::new("/")).is_some());
        assert!(!store.trusts(&home));
        // Even an entry for the home written by hand trusts nothing.
        std::fs::write(base.join("config/trusted.json"), serde_json::json!({"directories": [home, "/"]}).to_string()).unwrap();
        assert!(!store.trusts(&home) && !store.trusts(Path::new("/")));
        store.trust(&repo).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(base.join("config/trusted.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // R-TOOL-3: trust covers the MCP servers the repository named when
        // it was trusted; one added or changed later is asked about again.
        std::fs::write(base.join("config/trusted.json"), "{}").unwrap();
        std::fs::remove_file(repo.join(".mcp.json")).unwrap();
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo));
        std::fs::write(repo.join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "x"}}}"#).unwrap();
        assert!(!store.trusts(&repo), "r_tool_3: a .mcp.json that appeared after trust is asked about");
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo));
        std::fs::write(repo.join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "y"}}}"#).unwrap();
        assert!(!store.trusts(&repo), "r_tool_3: a changed .mcp.json is asked about");
        std::fs::write(base.join("config/trusted.json"), "not json").unwrap();
        assert!(!store.trusts(&repo), "a file that cannot be read trusts nothing");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn repo(name: &str) -> (PathBuf, PathBuf, Store) {
        let base = std::env::temp_dir().join(format!("krowk-trust-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        let repo = base.join("repo").canonicalize().unwrap();
        let store = Store::new(Some(base.join("config/trusted.json")), Some(base.join("home").canonicalize().unwrap()));
        (base, repo, store)
    }

    fn put(repo: &Path, file: &str, body: &str) {
        let p = repo.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// R-EASE-7: every file that can run code is fingerprinted. A change to
    /// any of them — a hook, a command, an agent, a skill, Codex's config —
    /// asks again and names the file; a file that runs nothing does not.
    #[test]
    fn r_ease_7_a_change_to_any_file_that_can_run_code_asks_again_and_names_it() {
        let files = [
            ".claude/settings.json",
            ".claude/hooks/pre.sh",
            ".claude/commands/deploy.md",
            ".claude/agents/reviewer.md",
            ".claude/skills/x/SKILL.md",
            ".krowk/agents/a.md",
            ".codex/config.toml",
            ".codex/hooks.json",
            ".codex/rules/default.rules",
            ".krowk/config.json",
            "packages/app/.claude/settings.json",
            "packages/app/.codex/config.toml",
            "packages/app/.krowk/config.json",
        ];
        for file in files {
            let (base, repo, store) = repo("each");
            for f in files {
                put(&repo, f, "one");
            }
            store.trust(&repo).unwrap();
            assert!(store.trusts(&repo) && store.changes(&repo).is_empty());
            put(&repo, "README.md", "unrelated");
            put(&repo, "src/main.rs", "unrelated");
            put(&repo, ".claude/notes.txt", "unrelated");
            assert!(store.trusts(&repo), "an unrelated file does not ask again");
            put(&repo, file, "two");
            assert!(!store.trusts(&repo), "{file}: changed, so asked again");
            assert_eq!(store.changes(&repo), [file], "{file} is named, and nothing else");
            assert!(store.untrusted(&repo, "Pass --trust.").message.contains(file));
            assert!(store.directories().is_empty(), "{file}: no longer announced as trusted");
            store.trust(&repo).unwrap();
            assert!(store.trusts(&repo), "{file}: trusting again takes the new version");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// A dependency folder is skipped only while git tracks nothing in it: a
    /// committed `venv/.claude/settings.json` is fingerprinted like any other.
    #[test]
    fn r_ease_7_a_dependency_folder_is_skipped_unless_git_tracks_a_file_in_it() {
        let (base, repo, store) = repo("deps");
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&repo).status().unwrap().success());
        git(&["init", "-q"]);
        put(&repo, "venv/.claude/settings.json", "one");
        put(&repo, "node_modules/x/.claude/settings.json", "one");
        store.trust(&repo).unwrap();
        put(&repo, "venv/.claude/settings.json", "two");
        put(&repo, "node_modules/x/.claude/settings.json", "two");
        assert!(store.trusts(&repo), "untracked dependency folders are not looked into");
        git(&["add", "venv/.claude/settings.json"]);
        assert!(!store.trusts(&repo), "a tracked file in one is fingerprinted");
        store.trust(&repo).unwrap();
        put(&repo, "venv/.claude/settings.json", "three");
        assert_eq!(store.changes(&repo), ["venv/.claude/settings.json"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_ease_7_a_file_that_appears_after_trust_asks_again() {
        let (base, repo, store) = repo("new");
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo));
        put(&repo, ".codex/config.toml", "[hooks]");
        assert!(!store.trusts(&repo));
        assert_eq!(store.changes(&repo), [".codex/config.toml"]);
        store.trust(&repo).unwrap();
        // Deleting one is not a way to run something.
        std::fs::remove_file(repo.join(".codex/config.toml")).unwrap();
        assert!(store.trusts(&repo));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_ease_7_a_link_inside_the_repository_is_followed_and_one_outside_is_not() {
        let (base, repo, store) = repo("links");
        put(&repo, "tools/codex/config.toml", "a");
        put(&repo, "tools/hook.sh", "a");
        #[cfg(unix)]
        {
            std::fs::create_dir_all(repo.join(".claude")).unwrap();
            std::os::unix::fs::symlink(repo.join("tools/codex"), repo.join(".codex")).unwrap();
            std::os::unix::fs::symlink(repo.join("tools/hook.sh"), repo.join(".claude/settings.json")).unwrap();
            put(&base, "outside.sh", "a");
            std::os::unix::fs::symlink(base.join("outside.sh"), repo.join(".claude/hooks")).unwrap();
            store.trust(&repo).unwrap();
            assert!(store.trusts(&repo));
            put(&repo, "tools/codex/config.toml", "b");
            assert_eq!(store.changes(&repo), [".codex/config.toml"], "the target's file changed");
            store.trust(&repo).unwrap();
            put(&repo, "tools/hook.sh", "b");
            assert_eq!(store.changes(&repo), [".claude/settings.json"]);
            store.trust(&repo).unwrap();
            put(&base, "outside.sh", "b");
            assert!(store.trusts(&repo), "a target outside the repository is not read");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A deep tree is not a change, and a directory link back up the tree
    /// ends in the walk's limit instead of a walk of every route.
    #[test]
    fn r_ease_7_a_deep_tree_stays_trusted_and_a_link_loop_ends() {
        let (base, repo, store) = repo("deep");
        put(&repo, "src/main/java/com/example/app/service/deeper/X.java", "x");
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo), "a deep tree is not a change");
        #[cfg(unix)]
        {
            std::fs::create_dir_all(repo.join(".claude/skills")).unwrap();
            for n in 0..10 {
                std::os::unix::fs::symlink(&repo, repo.join(format!(".claude/skills/s{n}"))).unwrap();
            }
            let started = std::time::Instant::now();
            let _ = store.trusts(&repo);
            assert!(started.elapsed() < std::time::Duration::from_secs(20), "the loop ended");
            assert!(store.changes(&repo).iter().any(|c| c == TRUNCATED), "and is not taken as unchanged");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repository too big to fingerprint is never remembered as unchanged.
    #[test]
    fn r_ease_7_a_repository_too_big_to_fingerprint_asks_again() {
        let (base, repo, store) = repo("big");
        for i in 0..=MAX_FILES {
            put(&repo, &format!(".claude/hooks/h{i}"), "x");
        }
        store.trust(&repo).unwrap();
        assert!(!store.trusts(&repo));
        assert!(store.changes(&repo).iter().any(|c| c == TRUNCATED));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Local-only settings are not in git and are the person's own: Claude
    /// Code's `settings.local.json` never asks again.
    #[test]
    fn r_ease_7_local_only_settings_do_not_ask_again() {
        let (base, repo, store) = repo("local");
        put(&repo, ".claude/settings.json", "{}");
        store.trust(&repo).unwrap();
        put(&repo, ".claude/settings.local.json", r#"{"hooks": {"x": 1}}"#);
        assert!(store.trusts(&repo));
        put(&repo, ".claude/settings.local.json", r#"{"hooks": {"x": 2}}"#);
        assert!(store.trusts(&repo) && store.changes(&repo).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A record from before files were fingerprinted upgrades without
    /// asking for the files that have not changed since it was made, and
    /// asks about the ones that have.
    #[test]
    fn r_ease_7_an_existing_record_upgrades_silently_for_files_unchanged_since() {
        let (base, repo, store) = repo("legacy");
        put(&repo, ".claude/settings.json", "{}");
        put(&repo, ".codex/config.toml", "a");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in [".claude/settings.json", ".codex/config.toml"] {
            std::fs::File::options().write(true).open(repo.join(f)).unwrap().set_modified(old).unwrap();
        }
        // The record as an older krowk wrote it: no `files`.
        let legacy = || {
            std::fs::create_dir_all(base.join("config")).unwrap();
            std::fs::write(base.join("config/trusted.json"), serde_json::json!({"directories": [repo]}).to_string()).unwrap();
        };
        legacy();
        assert!(store.trusts(&repo), "nothing changed since the record was made");
        // Nor does trusting something else (which rewrites the list) change that.
        std::fs::create_dir_all(base.join("other/.git")).unwrap();
        store.trust(&base.join("other").canonicalize().unwrap()).unwrap();
        assert!(store.trusts(&repo) && store.changes(&repo).is_empty());
        let raw = std::fs::read_to_string(base.join("config/trusted.json")).unwrap();
        assert!(raw.contains(".codex/config.toml"), "the record now carries fingerprints: {raw}");
        put(&repo, ".codex/config.toml", "b");
        assert_eq!(store.changes(&repo), [".codex/config.toml"], "a later change is caught by the fingerprint");

        // A file changed after the record was made, before anything upgraded it.
        legacy();
        std::fs::write(repo.join(".claude/settings.json"), r#"{"hooks": {}}"#).unwrap();
        assert!(!store.trusts(&repo));
        assert_eq!(store.changes(&repo), [".claude/settings.json"], "only the file that changed since, not .codex/config.toml");
        // The same when another repository's trust rewrites the list first.
        store.trust(&base.join("other").canonicalize().unwrap()).unwrap();
        assert_eq!(store.changes(&repo), [".claude/settings.json"]);
        store.trust(&repo).unwrap();
        assert!(store.trusts(&repo));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// R-HOST-3: the directories a host announces are the listed ones trust
    /// still holds for — never the home directory or `/`, even written into
    /// the file by hand, nor one whose `.mcp.json` changed since.
    #[test]
    fn r_host_3_directories_are_the_trusted_repositories_never_home_or_root() {
        let base = std::env::temp_dir().join(format!("krowk-trust-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["a/.git", "b/.git", "home"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let (a, b, home) = (base.join("a").canonicalize().unwrap(), base.join("b").canonicalize().unwrap(), base.join("home").canonicalize().unwrap());
        let store = Store::new(Some(base.join("config/trusted.json")), Some(home.clone()));
        assert!(store.directories().is_empty(), "no file, nothing");
        store.trust(&a).unwrap();
        store.trust(&b).unwrap();
        assert_eq!(store.directories(), [a.clone(), b.clone()]);
        std::fs::write(b.join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "x"}}}"#).unwrap();
        assert_eq!(store.directories(), std::slice::from_ref(&a), "b's servers changed since it was trusted");
        std::fs::write(base.join("config/trusted.json"), serde_json::json!({"directories": [home, "/", a]}).to_string()).unwrap();
        assert_eq!(store.directories(), [a], "home and / never count");
        std::fs::write(base.join("config/trusted.json"), "not json").unwrap();
        assert!(store.directories().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
