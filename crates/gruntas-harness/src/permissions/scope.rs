//! Where the file tools may reach (`Scope`): the working directory, the
//! directories the settings add, the fenced and secret directories, and
//! where a path really leads. The permission evaluator judges every call a
//! backend asks about against it.

use std::path::{Path, PathBuf};

/// `path` against `cwd`, unless it is absolute.
pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

/// Files a deny rule keeps from being read, which a search skips rather
/// than shows: `Read(.env)` denied holds for grep over the directory too.
#[derive(Clone, Default)]
pub struct Hidden(pub Option<HideFn>);

/// Whether a path is one a deny rule keeps from being read.
pub type HideFn = std::sync::Arc<dyn Fn(&Path) -> bool + Send + Sync>;

impl Hidden {
    pub fn hides(&self, p: &Path) -> bool {
        self.0.as_ref().is_some_and(|f| f(p))
    }
}

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Hidden(rules)" } else { "Hidden(none)" })
    }
}

/// Where the file tools may reach: the working directory and the
/// directories the settings add to it (`additionalDirectories`), and for reading
/// the skills' directories too. A path is judged by where it really leads —
/// `..`, an absolute path, a symlinked directory or file, a dangling
/// symlink a write would follow — never by how it is spelled. The
/// permission evaluator judges a call against this scope first; the tools
/// hold the scope the verdict opened.
#[derive(Debug, Clone)]
pub struct Scope {
    pub cwd: PathBuf,
    /// More directories the file tools reach as they reach the working
    /// directory.
    pub roots: Vec<PathBuf>,
    /// Directories they may read and never change.
    pub read_roots: Vec<PathBuf>,
    /// Reaching outside all of those is allowed: by bypassPermissions, an
    /// allow rule, or a person.
    pub outside: bool,
    /// The fenced directories are open: bypassPermissions, or a person
    /// approved this call.
    pub open: bool,
    /// More directories no edit may reach unless they are open, beside
    /// `.git`, `.claude`, `.codex` and `.gruntas`: gruntas's own config
    /// directory, Claude Code's, a backend instance's (`CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`), whose settings and hooks decide what runs next.
    pub protected: Vec<PathBuf>,
    /// Directories no tool reads, searches or changes without a person's
    /// say, wherever they sit: gruntas's home, as named and as it leads — its
    /// keys, logins, sessions and settings.
    pub secrets: Vec<PathBuf>,
    pub hidden: Hidden,
    /// The OS sandbox the session runs in (R-PERM-3): `bash` runs inside
    /// it, and the file tools hold its lines themselves — no rule, grant,
    /// person or mode opens them.
    pub sandbox: Option<std::sync::Arc<crate::sandbox::Plan>>,
}

/// Where one path leads, for the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Inside,
    /// Outside the working directory and its added directories: why.
    Outside(String),
    /// Inside a fenced directory: why.
    Fenced(String),
}

/// Symlinks followed at most while resolving one path: a loop is refused,
/// not followed forever.
const MAX_LINKS: usize = 40;

/// The directories the file tools keep out of, whichever repository they
/// are in.
const FENCED: [&str; 4] = [".git", ".claude", ".codex", ".gruntas"];

/// The fenced directory (`FENCED`) one of `path`'s components names, if
/// any: what the file tools change only with a person's say, and what
/// applying a worktree's changes back leaves to a person (WT13). Compared
/// the way the file system may: case-insensitively (macOS and Windows open
/// `.Claude` as `.claude`), and with the trailing dots and spaces Windows
/// drops (`.git.` is `.git`) — on every OS, since a checkout travels
/// between them.
pub fn fenced_dir(path: &Path) -> Option<&'static str> {
    let fold = |c: &std::ffi::OsStr| c.to_string_lossy().trim_end_matches(['.', ' ']).to_ascii_lowercase();
    path.components().find_map(|c| FENCED.into_iter().find(|d| fold(c.as_os_str()) == *d))
}

impl Scope {
    /// Only the working directory, nothing opened.
    pub fn within(cwd: &Path) -> Scope {
        Scope { cwd: cwd.to_path_buf(), roots: Vec::new(), read_roots: Vec::new(), outside: false, open: false, protected: Vec::new(), secrets: Vec::new(), hidden: Hidden::default(), sandbox: None }
    }

    /// Where `p` (absolute) leads against the tools' reach, for a read or an
    /// edit, before anything is opened.
    pub fn reach(&self, p: &Path, edit: bool) -> Reach {
        let real = match real_path(p, 0) {
            Ok(r) => r,
            Err(e) => return Reach::Outside(format!("{} cannot be resolved: {e}", p.display())),
        };
        let canon = |d: &Path| d.canonicalize().unwrap_or_else(|_| d.to_path_buf());
        let root = canon(&self.cwd);
        let mut roots: Vec<PathBuf> = vec![root.clone()];
        roots.extend(self.roots.iter().map(|d| canon(d)));
        if !edit {
            roots.extend(self.read_roots.iter().map(|d| canon(d)));
        }
        let inside = roots.iter().any(|r| real.starts_with(r));
        if self.secret(p) {
            return Reach::Fenced(format!("{} is inside gruntas's home, which holds its keys, logins and sessions: the tools read, search or change it only with a person's say", p.display()));
        }
        if edit && let Some(why) = self.fence(p, &real, &root) {
            return Reach::Fenced(why);
        }
        if inside {
            return Reach::Inside;
        }
        let leads = if real == p { String::new() } else { format!(" (it leads to {})", real.display()) };
        Reach::Outside(format!("{}{leads} is outside the working directory {}", p.display(), root.display()))
    }

    /// The reason `p` may not be changed without a person's say, if it may
    /// not: inside a `.git`, `.claude`, `.codex` or `.gruntas` directory, as
    /// spelled or as it leads, or inside a `protected` one. git runs what
    /// its config names (`core.fsmonitor`, hooks), Claude Code what its
    /// settings, hooks and agents name, Codex what its project config, rules
    /// and hooks name, gruntas what its own settings and hooks name — so a
    /// model that could write `.git/config` or `.claude/settings.json`
    /// could run any command without the bash permission. Codex keeps
    /// `.git` read-only for the same reason.
    fn fence(&self, p: &Path, real: &Path, root: &Path) -> Option<String> {
        let inside = fenced_dir;
        let rel = |q: &Path| {
            let mut bases: Vec<PathBuf> = vec![self.cwd.clone(), root.to_path_buf()];
            bases.extend(self.roots.iter().cloned());
            bases.iter().find_map(|b| q.strip_prefix(b).ok().map(Path::to_path_buf)).unwrap_or_else(|| q.to_path_buf())
        };
        if let Some(dir) = inside(&rel(p)).or_else(|| inside(&rel(real))) {
            let runs = match dir {
                ".git" => "git runs what its config and hooks name, so it is left to git itself",
                ".claude" => "Claude Code runs what its settings, hooks and agents name",
                ".codex" => "Codex runs what its project config, rules and hooks name",
                _ => "gruntas runs what its project config's hooks name, and its rules decide what else runs",
            };
            return Some(format!("{} is inside a {dir} directory, which the file tools change only with a person's say: {runs}", p.display()));
        }
        // Judged as spelled and as it leads, against the directory as named
        // and as it leads: a home reached through a symlink, or a link inside
        // the working directory that points into the home, is caught either way.
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        let within = |q: &Path, d: &Path| {
            let q = lower(q);
            q.starts_with(lower(d)) || q.starts_with(lower(&d.canonicalize().unwrap_or_else(|_| d.to_path_buf())))
        };
        self.protected.iter().find(|d| within(real, d) || within(p, d)).map(|d| {
            format!("{} is inside {}, a directory whose settings decide what runs (gruntas's, or a backend's own), which the file tools change only with a person's say", p.display(), d.display())
        })
    }

    /// Whether `p` is, or is inside, one of `secrets` — by where it leads
    /// (`..` taken out by its words, and every symlink followed), against
    /// the secret as named and as it leads, regardless of case: a search
    /// through `.gruntas/../.gruntas` or a symlinked alias skips it too.
    pub fn secret(&self, p: &Path) -> bool {
        if self.secrets.is_empty() {
            return false;
        }
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        let seen: Vec<PathBuf> = [Some(gruntas_api::home::lexical(p)), real_path(p, 0).ok()].into_iter().flatten().map(|q| lower(&q)).collect();
        // The secrets as named and as they lead: `Policy::load` lists both,
        // so a search resolves only the path it is at, once.
        self.secrets.iter().any(|s| {
            let n = lower(&gruntas_api::home::lexical(s));
            seen.iter().any(|q| q.starts_with(&n))
        })
    }

    /// The path a tool was given, resolved against the working directory,
    /// or why the tool may not touch it. The path is returned as spelled
    /// (joined to the working directory), so messages name what the model
    /// asked for; the check is on where it leads.
    ///
    /// Under a sandbox the path is resolved once, and that — where it
    /// leads — is what is judged and what is returned: the tool then opens
    /// it following no symlink (`exact`), so one swapped in after the check
    /// fails the open rather than leading it elsewhere.
    pub fn path(&self, path: &str) -> Result<PathBuf, String> {
        let p = resolve(&self.cwd, path);
        if let Some(plan) = &self.sandbox {
            let real = real_path(&p, 0).map_err(|e| format!("{} cannot be resolved: {e}", p.display()))?;
            if plan.hides(&real) {
                return Err(format!("{} is hidden by the {} sandbox, which keeps credentials, the home outside the workspace and gruntas's home out of reach: nothing opens it while gruntas runs sandboxed", p.display(), plan.profile.name()));
            }
            return match self.outside {
                true => Ok(real),
                false => match self.reach(&real, false) {
                    Reach::Inside | Reach::Fenced(_) => Ok(real),
                    Reach::Outside(why) => Err(format!("{why}: the file tools reach only inside the working directory and the directories the settings add, unless a person allows it, an allow rule covers it, or gruntas runs with `--permission-mode bypassPermissions`")),
                },
            };
        }
        if self.outside {
            return Ok(p);
        }
        match self.reach(&p, false) {
            Reach::Inside | Reach::Fenced(_) => Ok(p),
            Reach::Outside(why) => Err(format!("{why}: the file tools reach only inside the working directory and the directories the settings add, unless a person allows it, an allow rule covers it, or gruntas runs with `--permission-mode bypassPermissions`")),
        }
    }

    /// `path`, for a tool that changes the file: also never inside a fenced
    /// or `protected` directory unless the scope is open.
    pub fn edit_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = resolve(&self.cwd, path);
        if let Some(plan) = &self.sandbox {
            // The sandbox's lines, which nothing opens: the file tools write
            // where a sandboxed command could, and nowhere else — judged
            // and returned as the path leads, once (see `path`).
            // The fences as the workspace holds them now: a repository or a
            // configured hooks directory made since the scope was.
            let plan = &plan.current();
            if let Some(why) = &plan.refused {
                return Err(format!("{} was not changed: {why}", p.display()));
            }
            let real = real_path(&p, 0).map_err(|e| format!("{} cannot be resolved: {e}", p.display()))?;
            let why = match self.reach(&real, true) {
                _ if !plan.profile.writes() => format!("the {} sandbox writes nothing", plan.profile.name()),
                Reach::Inside if plan.fences(&real) => format!("{} is inside a git directory or a hooks directory a repository names, which the sandbox keeps read-only", p.display()),
                Reach::Inside => return Ok(real),
                Reach::Fenced(why) | Reach::Outside(why) => why,
            };
            return Err(format!("{why} — gruntas runs in the {} sandbox, which no rule, person or mode opens (R-PERM-3)", plan.profile.name()));
        }
        match self.reach(&p, true) {
            Reach::Inside => Ok(p),
            Reach::Fenced(_) if self.open => Ok(p),
            Reach::Outside(_) if self.outside => Ok(p),
            Reach::Fenced(why) => Err(format!("{why} — ask the person, or rerun with `--permission-mode bypassPermissions`")),
            Reach::Outside(why) => Err(format!("{why}: the file tools reach only inside the working directory and the directories the settings add, unless a person allows it, an allow rule covers it, or gruntas runs with `--permission-mode bypassPermissions`")),
        }
    }
}

/// Where a path leads once every symlink in it is followed, for a path
/// that need not exist yet: the nearest part that exists is canonicalized,
/// and the rest, which cannot hold a symlink, is appended. A dangling
/// symlink is followed to where it points, since a write through it would
/// create that.
pub(crate) fn real_path(p: &Path, links: usize) -> Result<PathBuf, String> {
    use std::path::Component;
    if links > MAX_LINKS {
        return Err("too many levels of symbolic links".into());
    }
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(p).map_err(|e| e.to_string())?;
            let target = if target.is_absolute() { target } else { p.parent().unwrap_or(Path::new("/")).join(target) };
            real_path(&target, links + 1)
        }
        Ok(_) => Err("it exists but cannot be resolved".into()),
        Err(_) => {
            let parent = p.parent().ok_or("no parent directory")?;
            let base = real_path(parent, links)?;
            match p.components().next_back() {
                Some(Component::ParentDir) => Ok(base.parent().map(Path::to_path_buf).unwrap_or(base)),
                Some(Component::CurDir) | None => Ok(base),
                Some(c) => Ok(base.join(c.as_os_str())),
            }
        }
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod scope_tests;
