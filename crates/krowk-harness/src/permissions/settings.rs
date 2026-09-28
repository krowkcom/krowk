//! Where permission rules, directories and hooks come from, and how much
//! each place is believed (R-PERM-1).
//!
//! | source | file | believed |
//! |---|---|---|
//! | krowk, user | `config.json` in krowk's home: `permissions`, `hooks` | in full |
//! | Claude Code, user | `settings.json` in Claude's config directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) | in full |
//! | krowk, remembered | `permissions.json` in krowk's home: what a person allowed for a project | in full, for that project |
//! | krowk, project | `<repository>/.krowk/config.json` | trusted repository: in full; otherwise its `deny` and `ask` only |
//! | Claude Code, project | `.claude/settings.json` and `.claude/settings.local.json`, from the repository's root down to the working directory | the same |
//!
//! Every file has Claude Code's shape — `permissions.allow`, `.ask`,
//! `.deny` (lists of rules), `.defaultMode`, `.additionalDirectories`, and
//! `hooks` — so a repository set up for Claude Code needs nothing new.
//!
//! **A repository cannot widen what it may do until it is trusted.** Its
//! files are someone else's until the person says otherwise (R-BACK-6's
//! question, the same trusted list): what only narrows — a deny rule, an
//! ask rule — always applies, and what widens — an allow rule, a directory
//! outside the repository, a `defaultMode`, a hook, which is a command —
//! applies only once the repository is trusted. Even then a repository's
//! `defaultMode` is never `bypassPermissions` or `unhinged`: that is the
//! person's to choose, on the command line or in their own settings. None
//! of these files is one the model can write: every file tool refuses
//! `.claude`, `.krowk` and krowk's home unless a person approves that one
//! call (or runs unhinged), and no allow rule or remembered grant opens
//! them.
//!
//! A file that exists and does not parse, or holds a rule that does not,
//! stops the turn with its name: a deny rule silently dropped is a rule
//! that no longer holds. A `defaultMode` krowk does not run (Claude Code's
//! `auto`, or one added later) is the one exception: a mode can only widen,
//! so the file is read as `default` — never looser — and a notice names
//! the file and the value.

use super::rules::{self, Kind, Rule};
use crate::hooks::{self, Hooks};
use crate::protocol::PermissionMode;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Whether a repository root has been trusted, by the client's rules
/// (krowk's trusted list, `--trust`).
pub type Trusted = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// What the host is told about the person's own settings and the places
/// krowk keeps its own. Everything is optional: a host with none of it
/// judges by the modes alone.
#[derive(Clone, Default)]
pub struct Config {
    /// krowk's `config.json`, whole: its `permissions` and `hooks` are read.
    pub user: Option<Value>,
    /// Where it lives, for messages.
    pub user_path: Option<PathBuf>,
    /// Read `user_path` again at every load instead of `user`: a host that
    /// outlives an edit of config.json (the daemon) holds each turn to the
    /// deny rules and hooks the file has now.
    pub reread: bool,
    /// The person's home: `~` in rules, and where Claude's user files are
    /// found when `claude_dir` is unset.
    pub home: Option<PathBuf>,
    /// Claude Code's user config directory (`$CLAUDE_CONFIG_DIR`, else
    /// `~/.claude`): `settings.json`, `CLAUDE.md`, `skills/`.
    pub claude_dir: Option<PathBuf>,
    /// krowk's home: remembered grants live there, and no file
    /// tool writes into it — its config, trust list and grants decide what
    /// krowk allows.
    pub krowk_dir: Option<PathBuf>,
    /// Which repositories are trusted; none trusts none.
    pub trusted: Option<Trusted>,
    /// A client is attached that answers approval requests (the TUI). A
    /// host without one — `krowk -p` — never waits on a person: what would
    /// be asked is refused, with the reason.
    pub approvals: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config").field("user_path", &self.user_path).field("home", &self.home).field("claude_dir", &self.claude_dir).field("krowk_dir", &self.krowk_dir).field("approvals", &self.approvals).finish()
    }
}

impl Config {
    /// Claude Code's user directory: the one named, else `~/.claude`.
    pub fn claude_home(&self) -> Option<PathBuf> {
        self.claude_dir.clone().or_else(|| self.home.as_ref().map(|h| h.join(".claude")))
    }

    /// The remembered-grants file.
    pub fn grants_file(&self) -> Option<PathBuf> {
        self.krowk_dir.as_ref().map(|d| d.join(GRANTS_FILE))
    }
}

/// The remembered project grants, in krowk's home.
pub const GRANTS_FILE: &str = "permissions.json";

/// Everything the sources say for one working directory.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    /// Every rule that applies, with what it says.
    pub rules: Vec<(Kind, Rule)>,
    /// Allow rules of an untrusted repository, kept only to say why a call
    /// they would have allowed was asked about.
    pub ignored_allow: Vec<Rule>,
    /// `additionalDirectories`, resolved.
    pub dirs: Vec<PathBuf>,
    /// The most specific `defaultMode`.
    pub default_mode: Option<PermissionMode>,
    /// What the person is told of the files that count: a `defaultMode`
    /// krowk does not run.
    pub notices: Vec<String>,
    pub hooks: Hooks,
    /// The repository the working directory belongs to.
    pub root: PathBuf,
    pub trusted: bool,
    /// The repository has settings that widen, which it would take trust
    /// to apply: what the trust question is for.
    pub widens: bool,
}

/// One settings file's worth, before it is believed or not.
#[derive(Debug, Default)]
struct File {
    source: String,
    rules: Vec<(Kind, Rule)>,
    dirs: Vec<PathBuf>,
    default_mode: Option<PermissionMode>,
    /// A `defaultMode` krowk does not run, as JSON text, and whether it
    /// counts as `default` (anything but Claude Code's `auto`, which sets
    /// nothing).
    unknown_mode: Option<(String, bool)>,
    hooks: Hooks,
}

impl File {
    /// The mode this file sets, and what is said of a mode krowk does not
    /// run. `auto` sets nothing, and is spoken of only if no file sets a
    /// mode (`load` decides); anything else is `default`, said at once.
    fn mode(&self, home: Option<&Path>, config: &str) -> (Option<PermissionMode>, Option<String>) {
        let Some((m, narrows)) = &self.unknown_mode else { return (self.default_mode, None) };
        if m == "\"unhinged\"" {
            let why = format!("{} sets defaultMode \"unhinged\", which is krowk's own — Claude Code skips a settings file naming it, deny rules and hooks included — so krowk asks before edits and commands · set permissions.defaultMode in {config} to choose unhinged", tilde(&self.source, home));
            return (Some(PermissionMode::Default), Some(why));
        }
        let why = format!("{} sets defaultMode {m}, which krowk doesn't have, so it asks before edits and commands · set permissions.defaultMode in {config} to choose", tilde(&self.source, home));
        (narrows.then_some(PermissionMode::Default), Some(why))
    }
}

/// `~/…` for a path under the home directory.
fn tilde(path: &str, home: Option<&Path>) -> String {
    match home.and_then(|h| Path::new(path).strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    }
}

/// Reads one settings object.
fn read_object(v: &Value, source: &str, root: &Path, base: &Path, home: Option<&Path>) -> Result<File, String> {
    let mut f = File { source: source.to_string(), ..File::default() };
    if let Some(p) = v.get("permissions") {
        let p = p.as_object().ok_or_else(|| format!("{source}: \"permissions\" must be an object"))?;
        for (key, kind) in [("allow", Kind::Allow), ("ask", Kind::Ask), ("deny", Kind::Deny)] {
            let Some(list) = p.get(key) else { continue };
            let list = list.as_array().ok_or_else(|| format!("{source}: \"permissions.{key}\" must be a list of rules"))?;
            for r in list {
                let text = r.as_str().ok_or_else(|| format!("{source}: every rule in \"permissions.{key}\" must be a string"))?;
                f.rules.push((kind, rules::parse(text, source, root).map_err(|e| format!("{source}: permissions.{key}: {e}"))?));
            }
        }
        // Claude Code's `auto` — looser than default, a classifier deciding
        // — sets nothing, so another file's mode still counts. Anything
        // else krowk does not run (`dontAsk`, a mode from a later Claude
        // Code, not a string at all) may have meant stricter, so it is read
        // as default, and an earlier file's looser mode does not stand.
        match p.get("defaultMode") {
            None | Some(Value::Null) => {}
            Some(Value::String(m)) => match PermissionMode::parse(m) {
                Some(mode) => f.default_mode = Some(mode),
                None if m == "auto" => f.unknown_mode = Some((format!("{m:?}"), false)),
                None => f.unknown_mode = Some((format!("{m:?}"), true)),
            },
            Some(other) => f.unknown_mode = Some((other.to_string(), true)),
        }
        if let Some(dirs) = p.get("additionalDirectories") {
            let dirs = dirs.as_array().ok_or_else(|| format!("{source}: \"permissions.additionalDirectories\" must be a list of paths"))?;
            for d in dirs {
                let d = d.as_str().ok_or_else(|| format!("{source}: every entry of \"permissions.additionalDirectories\" must be a path"))?;
                f.dirs.push(resolve_dir(d, base, home));
            }
        }
    }
    if let Some(h) = v.get("hooks") {
        f.hooks = hooks::parse(h, source)?;
    }
    Ok(f)
}

/// A directory as settings name it: `~/…` under the home, relative to
/// where the settings apply, or absolute.
fn resolve_dir(d: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    let p = match (d.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ if d == "~" => home.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(d)),
        _ => {
            let p = Path::new(d);
            if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
        }
    };
    p.canonicalize().unwrap_or(p)
}

/// Reads a settings file, if there is one. A file that is there and cannot
/// be read or parsed is an error, never an empty file.
fn read_file(path: &Path, root: &Path, base: &Path, home: Option<&Path>) -> Result<Option<File>, String> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Ok(None),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    read_object(&v, &path.display().to_string(), root, base, home).map(Some)
}

/// The directories from the repository's root down to the working
/// directory, root first.
pub fn chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut dirs: Vec<PathBuf> = cwd.ancestors().take_while(|d| d.starts_with(root)).map(Path::to_path_buf).collect();
    if dirs.is_empty() {
        dirs.push(cwd);
    }
    dirs.reverse();
    dirs
}

/// Everything that applies in `cwd`.
pub fn load(cfg: &Config, cwd: &Path) -> Result<Loaded, String> {
    let root = crate::trust::root(cwd);
    let home = cfg.home.as_deref();
    let trusted = cfg.trusted.as_ref().is_some_and(|t| t(&root));
    let mut user: Vec<File> = Vec::new();
    let fresh = match (&cfg.user_path, cfg.reread) {
        (Some(p), true) => match std::fs::read(p) {
            Ok(raw) => Some(serde_json::from_slice::<Value>(&raw).map_err(|e| format!("{} is not valid JSON: {e}", p.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{} cannot be read: {e}", p.display())),
        },
        _ => cfg.user.clone(),
    };
    if let Some(v) = &fresh {
        let source = cfg.user_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "krowk's config.json".into());
        let base = cfg.home.clone().unwrap_or_else(|| cwd.to_path_buf());
        user.push(read_object(v, &source, &root, &base, home)?);
    }
    if let Some(dir) = cfg.claude_home()
        && let Some(mut f) = read_file(&dir.join("settings.json"), &root, cfg.home.as_deref().unwrap_or(&dir), home)?
    {
        // unhinged is krowk's own, and Claude Code skips a whole settings
        // file naming a mode it does not know — its deny rules and hooks
        // with it. So Claude's file never chooses unhinged: it reads as
        // default, with a notice, and krowk's config.json is where it is set.
        if f.default_mode == Some(PermissionMode::Unhinged) {
            f.default_mode = None;
            f.unknown_mode = Some(("\"unhinged\"".into(), true));
        }
        user.push(f);
    }
    if let Some(path) = cfg.grants_file() {
        user.extend(remembered(&path, &root)?);
    }
    let mut project: Vec<File> = Vec::new();
    if let Some(f) = read_file(&root.join(".krowk/config.json"), &root, &root, home)? {
        project.push(f);
    }
    for dir in chain(&root, cwd) {
        for name in ["settings.json", "settings.local.json"] {
            if let Some(f) = read_file(&dir.join(".claude").join(name), &root, &dir, home)? {
                project.push(f);
            }
        }
    }
    // The home directory's own `.claude/settings.json` is Claude's user
    // file, read once above, not a project's too.
    if let Some(dir) = cfg.claude_home() {
        let user_file = dir.join("settings.json").display().to_string();
        project.retain(|f| f.source != user_file);
    }
    let mut out = Loaded { root: root.clone(), trusted, ..Loaded::default() };
    let config = cfg.user_path.as_ref().map(|p| tilde(&p.display().to_string(), home)).unwrap_or_else(|| "krowk's config.json".into());
    // `auto`'s notices, said only if nothing sets a mode.
    let mut unknown: Vec<String> = Vec::new();
    // A mode krowk does not run, read as default, is said only while it is
    // the mode that runs: a later file's mode takes its notice with it.
    let mut narrowed: Option<String> = None;
    let mut said = |mode: Option<PermissionMode>, notice: Option<String>| match (mode, notice) {
        (Some(_), Some(n)) => narrowed = Some(n),
        (Some(_), None) => narrowed = None,
        (None, Some(n)) => unknown.push(n),
        (None, None) => {}
    };
    for f in user {
        let (mode, notice) = f.mode(home, &config);
        said(mode, notice);
        out.rules.extend(f.rules);
        out.dirs.extend(f.dirs);
        out.default_mode = mode.or(out.default_mode);
        out.hooks.extend(f.hooks);
    }
    for f in project {
        let widens = f.rules.iter().any(|(k, _)| *k == Kind::Allow) || !f.dirs.is_empty() || f.default_mode.is_some() || !f.hooks.is_empty();
        out.widens |= widens;
        if trusted {
            // A repository never puts the person in bypassPermissions, nor
            // unhinged.
            let (mode, notice) = f.mode(home, &config);
            said(mode.filter(|m| !m.asks_nothing()), notice);
            out.rules.extend(f.rules);
            out.dirs.extend(f.dirs);
            out.default_mode = mode.filter(|m| !m.asks_nothing()).or(out.default_mode);
            out.hooks.extend(f.hooks);
        } else {
            for (k, r) in f.rules {
                match k {
                    Kind::Allow => out.ignored_allow.push(r),
                    _ => out.rules.push((k, r)),
                }
            }
        }
    }
    out.notices.extend(narrowed);
    if out.default_mode.is_none() {
        out.notices.extend(unknown.pop());
    }
    Ok(out)
}

/// Whether a repository has settings that only trust would apply: the
/// trust question's reason, for a native session.
pub fn widens(cfg: &Config, cwd: &Path) -> bool {
    let cfg = Config { trusted: None, user: None, claude_dir: None, krowk_dir: None, ..cfg.clone() };
    load(&cfg, cwd).map(|l| l.widens).unwrap_or(false)
}

/// The grants remembered for the project at `root`.
fn remembered(path: &Path, root: &Path) -> Result<Vec<File>, String> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let Some(p) = v.pointer("/projects").and_then(|p| p.get(root.display().to_string())) else { return Ok(Vec::new()) };
    let source = format!("{} (remembered for {})", path.display(), root.display());
    Ok(vec![read_object(&serde_json::json!({ "permissions": p }), &source, root, root, None)?])
}

/// Holds the grants file's lock while it is read, changed and replaced:
/// two sessions granting at once each keep what the other wrote.
struct GrantsLock(#[allow(dead_code)] std::fs::File);

fn lock_grants(path: &Path) -> Result<GrantsLock, String> {
    let dir = path.parent().ok_or_else(|| format!("{} has no directory", path.display()))?;
    crate::log::private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let lock = path.with_extension("json.lock");
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(&lock).map_err(|e| format!("open {}: {e}", lock.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns; released when
        // the returned guard closes it.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("lock {}: {}", lock.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(GrantsLock(f))
}

/// Remembers `allow` for the project at `root`: `0600`, under a lock, and
/// replaced by rename from a temporary file of this process's own.
pub fn remember(path: &Path, root: &Path, allow: &[String]) -> Result<(), String> {
    // A rule that would not load is never written: the next session would
    // refuse the whole file.
    for r in allow {
        rules::parse(r, &path.display().to_string(), root).map_err(|e| format!("{} was not remembered: {e}", r))?;
    }
    let _held = lock_grants(path)?;
    let mut v: Value = match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let list = v
        .as_object_mut()
        .expect("an object")
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("{}: \"projects\" must be an object", path.display()))?
        .entry(root.display().to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("{}: a project's entry must be an object", path.display()))?
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let arr = list.as_array_mut().ok_or_else(|| format!("{}: \"allow\" must be a list", path.display()))?;
    for r in allow {
        if !arr.iter().any(|x| x.as_str() == Some(r)) {
            arr.push(Value::String(r.clone()));
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("json.{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let body = serde_json::to_vec_pretty(&v).expect("the grants serialize");
    std::io::Write::write_all(&mut o.open(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?, &body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))
}
