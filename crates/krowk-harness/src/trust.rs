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
//! never syncs. The native engine consults it for what a repository would
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

/// What a backend would run of the repository's own, found in it: the
/// reason the question is asked, shown with it.
pub fn what_runs(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for (file, what) in [
        (".claude/settings.json", "its hooks and settings"),
        (".claude/settings.local.json", "its local hooks and settings"),
        (".mcp.json", "its MCP servers"),
        (".claude/commands", "its commands"),
        (".claude/agents", "its agents"),
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
        unrecordable(root, self.home.as_deref()).is_none() && listed.directories.iter().any(|d| Path::new(d) == root)
    }

    /// Why `root` cannot be remembered, if it cannot.
    pub fn refuses(&self, root: &Path) -> Option<String> {
        unrecordable(root, self.home.as_deref())
    }

    /// Remembers `root` as trusted.
    pub fn trust(&self, root: &Path) -> Result<(), String> {
        if let Some(why) = self.refuses(root) {
            return Err(format!("{} is not remembered as trusted: {why}", root.display()));
        }
        let mut listed = self.read()?;
        let root = root.display().to_string();
        if listed.directories.contains(&root) {
            return Ok(());
        }
        listed.directories.push(root);
        listed.directories.sort();
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
        std::fs::write(base.join("config/trusted.json"), "not json").unwrap();
        assert!(!store.trusts(&repo), "a file that cannot be read trusts nothing");
        let _ = std::fs::remove_dir_all(&base);
    }
}
