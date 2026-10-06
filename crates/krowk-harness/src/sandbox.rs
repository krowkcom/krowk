//! The OS sandbox (R-PERM-3): the boundary a permission prompt is not.
//! A command the `bash` tool runs is started inside it, so what the
//! command does — and whatever it starts — meets the kernel's refusal,
//! not krowk's judgement of a command line it cannot fully read.
//!
//! What it lets a command see is allowed, not listed: the person's home
//! is replaced by an empty directory, and only the workspace and the Rust
//! toolchain's homes (read-only) are bound back into it. A home's
//! credentials live in more dotfiles than any list names, and under
//! `workspace`, which keeps the network, a readable `~/Documents` would be
//! as good a channel out as a readable `~/.ssh`.
//!
//! Three named profiles:
//!
//! - **workspace**: the working directory and the directories the
//!   settings add are writable; the rest of the file system outside the
//!   home is read-only; the network is on, the resolver with it.
//! - **read-only**: nothing is writable but a private `/tmp`; no network,
//!   and no resolver.
//! - **strict**: the workspace is writable; no network, and no resolver.
//!
//! In every profile every `.git` in the workspace — nested repositories
//! and gitdir files included, found by searching it when a call runs — and
//! every hooks directory a repository's `core.hooksPath` names inside it
//! stay read-only, as do `.claude`, `.codex` and `.krowk` at its top and
//! the settings directories inside it; a `.git` a command creates is
//! removed after the call. A command that could write `.git/hooks` or
//! `.claude/settings.json` could run anything the next time git or an
//! agent starts.
//!
//! One workspace gets more: a worktree krowk made for an agent (worktrees
//! WT4), which the agent must be able to commit in. A commit in a linked
//! worktree writes objects and refs into the repository's common git
//! directory, outside the workspace, so when the workspace is one
//! (`managed_worktree`) the plan binds the common directory read-only (it
//! may be in the hidden home), then `<common>/objects`, `refs/heads/krowk`
//! and `logs/refs/heads/krowk` (each made first if missing) and the
//! worktree's own admin directory writable, and then, read-only on top,
//! what in them decides
//! what runs or what another checkout is on: every other entry in
//! `refs/heads/krowk` (another agent's branch), and this worktree's
//! `config.worktree`, `commondir`, `gitdir` and `modules`. The rest of
//! `refs` stays read-only, not just the branches checked out somewhere: a
//! fence on `refs/heads/main` inside a writable `refs` is undone by
//! renaming `refs/heads` and making a new one (a mount point cannot be
//! renamed, but its parent directory can), and a `refs/replace/<commit>`
//! changes what `main` shows and checks out in the main checkout without
//! moving it. Only the `krowk/` reflogs are writable, not all of
//! `logs`, and the writable git directories are swept of anything that
//! is not a regular file or a directory before and after every call
//! (`sweep`; `objects` only at its top and in `pack` and `info`, where a
//! linked directory would take the objects git writes): git outside the
//! sandbox appends to a reflog in place, so a reflog a command replaced
//! with a symlink to `~/.bashrc` would have the person's next commit write
//! a line the agent chose there. So a command can make no tag, and no branch outside
//! `krowk/`. `packed-refs` stays read-only for the second reason: a line
//! appended to it is a ref, and git never rewrites it in place anyway (it
//! writes `packed-refs.lock` beside it, in the read-only common directory).
//! The fences above are listed in `read_only` with `<common>/config`,
//! `hooks` and any `core.hooksPath` target, `info`, `HEAD`, `modules`,
//! `shallow`, every other worktree's admin directory and the worktree's
//! `.git` file, so the file tools hold them too. A write inside a writable
//! directory cannot get round a fence the same way: each fence's parent is
//! itself a mount point. Being under `krowk_api::home::worktrees_root` is
//! not enough, since the environment names that directory: the worktree's
//! `.git` must lead to `<common>/worktrees/<name>` in a real common
//! directory outside the root, that admin directory must lead back to it,
//! and its branch must be `krowk/<8 hex>`. Anything else — a person's own
//! linked worktree included — keeps the fences above, and so does a
//! worktree whose `HEAD` a command moves off its `krowk/` branch, from the
//! next call on. The person's git identity is in their hidden home, so
//! `user.name` and `user.email` as git resolves them in the worktree are
//! read outside the sandbox and handed in as `GIT_AUTHOR_*` and
//! `GIT_COMMITTER_*`; the home's `.gitconfig` is never bound, since it can
//! hold credentials. Accepted: `objects` is writable, so a command can
//! rewrite an object file already there in place.
//!
//! `/tmp` is private to the call, and `/run` — where the ssh
//! agent, the session bus and the Docker socket live — is empty but for
//! the resolver under `workspace`. Cargo's registry credentials are hidden
//! in the cargo home in use and in `~/.cargo`; the toolchain homes are not
//! writable, so a build of what is already fetched works and a fetch does
//! not — the simpler of the safe choices. The command's environment is an
//! allowlist (`env`), it inherits no descriptor past stdio, and it runs in
//! a session, IPC, UTS and pid namespace of its own.
//!
//! It fails closed: a profile this machine cannot enforce refuses to run
//! anything, with a `fix`, rather than running unsandboxed. On Linux it is
//! bubblewrap (`bwrap`), probed by running it once, since a `bwrap` whose
//! user namespaces the kernel or AppArmor forbids is as good as none. On
//! macOS it will be Seatbelt (`sandbox-exec`), which is not built yet; on
//! Windows there is none. Both refuse.
//!
//! Inside a container (`By::Container`) the container holds the commands,
//! and krowk adds only the file tools' fences.
//!
//! The file tools run in krowk's own process, not in the sandbox: under
//! one they hold the same lines themselves (`tools::Scope::edit_path`,
//! `tools::Scope::path`, the same fences via `Plan::fences`), open exactly
//! the path they checked (`tools::exact`), and no rule, grant, person or
//! mode opens them.

use std::path::{Path, PathBuf};

/// A named sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Workspace,
    ReadOnly,
    Strict,
}

impl Profile {
    pub const NAMES: [&'static str; 3] = ["workspace", "read-only", "strict"];

    pub fn parse(s: &str) -> Option<Profile> {
        Some(match s {
            "workspace" => Profile::Workspace,
            "read-only" => Profile::ReadOnly,
            "strict" => Profile::Strict,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Workspace => "workspace",
            Profile::ReadOnly => "read-only",
            Profile::Strict => "strict",
        }
    }

    /// Whether the workspace may be written.
    pub fn writes(self) -> bool {
        self != Profile::ReadOnly
    }

    /// Whether the network is reachable.
    pub fn network(self) -> bool {
        self == Profile::Workspace
    }
}

/// What holds a session's tools to a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// bubblewrap, around every command; the file tools hold the same lines.
    Bubblewrap,
    /// The container krowk runs in, which is the command's boundary: the
    /// commands run as they are, and the file tools hold the profile's
    /// lines themselves all the same — the container does not keep them
    /// out of `.git/hooks` or the person's credentials.
    Container,
}

/// A session's sandbox: its profile, and what enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sandbox {
    pub profile: Profile,
    pub by: By,
}

/// Where a sandboxed command's home is: a directory of its private `/tmp`,
/// so what it keeps there — and what it would read there — is its own.
pub const HOME: &str = "/tmp/home";

/// Directories under the home holding what signs, logs in or decrypts as
/// the person: hidden in every profile.
const CREDENTIALS: [&str; 8] = [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".config/gcloud", ".config/gh"];

/// The directories inside a writable one that stay read-only: what git,
/// Claude Code, Codex and krowk run what they name from.
const FENCED: [&str; 4] = [".git", ".claude", ".codex", ".krowk"];

/// One sandbox, laid out: what a call may write, what it may only read,
/// what it may not see, and whether it reaches the network.
/// What a plan is laid out from, kept so it can be laid out again when a
/// call runs (`Plan::current`).
#[derive(Debug, Clone)]
struct Inputs {
    sandbox: Sandbox,
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    readable: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    secrets: Vec<PathBuf>,
    home: Option<PathBuf>,
    toolchains: Vec<(&'static str, PathBuf)>,
    worktrees: Option<PathBuf>,
    /// Where git finds the person's own config (`GIT_CONFIG_ENV`), as
    /// krowk's environment names it: a krowk worktree's identity is read
    /// with it.
    config_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    walk: Walk,
}

/// The variables that say where git reads the person's own config.
const GIT_CONFIG_ENV: [&str; 2] = ["HOME", "XDG_CONFIG_HOME"];

#[derive(Debug, Clone)]
pub struct Plan {
    inputs: Inputs,
    pub profile: Profile,
    /// Whether commands run inside bubblewrap; false inside a container,
    /// where only the file tools' own fences are krowk's to add.
    pub kernel: bool,
    pub cwd: PathBuf,
    /// Bound read-write (read-only under the read-only profile).
    pub writable: Vec<PathBuf>,
    /// A krowk worktree's common git directory (`managed_worktree`), in
    /// the order it is bound after `writable` and before `read_only`: the
    /// directory read-only, then what a commit writes in it (`true`)
    /// writable. Empty for every other workspace. Not the file tools' to
    /// write.
    pub git: Vec<(PathBuf, bool)>,
    /// The writable ones of `git` that git outside the sandbox later
    /// writes through — a reflog it appends to, a ref, the worktree's
    /// admin directory — swept of anything but regular files and
    /// directories before and after each call (`sweep`): each directory,
    /// and whether all of it (`true`) or only what is in it.
    pub swept: Vec<(PathBuf, bool)>,
    /// The person's `user.name` and `user.email` as git resolves them in a
    /// krowk worktree, handed to its commands as `GIT_AUTHOR_*` and
    /// `GIT_COMMITTER_*`: the config that names them is in the hidden home.
    pub identity: Option<(String, String)>,
    /// Bound read-only over what `writable` and `git` opened: `.git` and
    /// its kind, and the settings directories that decide what runs.
    pub read_only: Vec<PathBuf>,
    /// Replaced by an empty directory.
    pub hidden: Vec<PathBuf>,
    /// The person's home, replaced by an empty one in every profile before
    /// what is allowed back is bound into it: default-deny, since what a
    /// home holds that signs or logs in as the person (`.git-credentials`,
    /// `.netrc`, `.npmrc`, an agent's login) is no list's to name.
    pub home: Option<PathBuf>,
    /// Directories bound read-only back into a hidden home: the skills and
    /// the toolchain homes.
    pub readable: Vec<PathBuf>,
    /// Why no command may run in this plan, when one may not: a workspace
    /// too large to search for every repository in it is refused, since a
    /// `.git` not found is a `.git` not fenced.
    pub refused: Option<String>,
    /// Every `.git` the workspace search found, as it found them: what
    /// `Unfenced` compares the workspace with after a call.
    pub repos: Vec<PathBuf>,
    /// The Rust toolchain's homes, as `RUSTUP_HOME` and `CARGO_HOME` name
    /// them (or `~/.rustup` and `~/.cargo`): readable, never writable, and
    /// named to the command, since its own `HOME` is private. Cargo's
    /// registry credentials in them stay hidden.
    pub toolchains: Vec<(&'static str, PathBuf)>,
}

impl Plan {
    /// The plan for `profile` over a session's reach: its working
    /// directory and added directories (`roots`), the skills it reads
    /// (`readable`), the settings directories it keeps (`protected`) and
    /// krowk's home (`secrets`).
    pub fn new(sandbox: Sandbox, cwd: &Path, roots: &[PathBuf], readable: &[PathBuf], protected: &[PathBuf], secrets: &[PathBuf], home: Option<&Path>) -> Plan {
        Plan::new_in(sandbox, cwd, roots, readable, protected, secrets, home, &|k| std::env::var_os(k))
    }

    /// `new`, sharing `walk`, the workspace search of the turn the plan is
    /// for: its first call searches the workspace, and each later one
    /// sweeps what the first found for changes.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with(sandbox: Sandbox, cwd: &Path, roots: &[PathBuf], readable: &[PathBuf], protected: &[PathBuf], secrets: &[PathBuf], home: Option<&Path>, walk: Walk) -> Plan {
        let env = |k: &str| std::env::var_os(k);
        let (toolchains, worktrees, config_env) = (toolchains(home, &env), worktrees_root(&env), GIT_CONFIG_ENV.map(|k| (k, env(k))).to_vec());
        Plan::build(Inputs { sandbox, cwd: cwd.into(), roots: roots.into(), readable: readable.into(), protected: protected.into(), secrets: secrets.into(), home: home.map(Into::into), toolchains, worktrees, config_env, walk })
    }

    /// `new`, reading `RUSTUP_HOME`, `CARGO_HOME`, the worktrees root's
    /// variables and where git's own config is from `env`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_in(sandbox: Sandbox, cwd: &Path, roots: &[PathBuf], readable: &[PathBuf], protected: &[PathBuf], secrets: &[PathBuf], home: Option<&Path>, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Plan {
        let (toolchains, worktrees, config_env) = (toolchains(home, env), worktrees_root(env), GIT_CONFIG_ENV.map(|k| (k, env(k))).to_vec());
        Plan::build(Inputs { sandbox, cwd: cwd.into(), roots: roots.into(), readable: readable.into(), protected: protected.into(), secrets: secrets.into(), home: home.map(Into::into), toolchains, worktrees, config_env, walk: Walk::default() })
    }

    /// The plan laid out again, as the workspace and the home are now: a
    /// call's command and a file tool's check see the repositories, hooks
    /// directories and home entries there are when it runs, not those
    /// there were when the session's scope was made.
    pub fn current(&self) -> Plan {
        Plan::build(self.inputs.clone())
    }

    fn build(inputs: Inputs) -> Plan {
        let Inputs { sandbox, ref cwd, ref roots, ref readable, ref protected, ref secrets, ref home, ref toolchains, ref worktrees, ref config_env, ref walk } = inputs;
        let (cwd, home) = (cwd.as_path(), home.as_deref());
        // As they lead: a bind mount is of the real directory, and a
        // symlinked working directory would otherwise leave `.git` where
        // the mount never reaches.
        let canon = |d: &Path| d.canonicalize().unwrap_or_else(|_| d.to_path_buf());
        let mut writable: Vec<PathBuf> = Vec::new();
        for d in std::iter::once(cwd).chain(roots.iter().map(PathBuf::as_path)).map(canon) {
            if !writable.contains(&d) {
                writable.push(d);
            }
        }
        let mut read_only: Vec<PathBuf> = writable.iter().flat_map(|w| FENCED.iter().map(move |f| w.join(f))).collect();
        // Every repository in the workspace, nested ones too, and the hooks
        // directory each names: bound read-only, and fenced from the file
        // tools alike.
        let (fences, refused) = match walk.lock().unwrap_or_else(|e| e.into_inner()).repositories(&writable, home) {
            Ok(r) => (r, None),
            Err(why) => (Fences::default(), Some(why)),
        };
        let repos = fences.repos.clone();
        read_only.extend(fences.all());
        // A worktree krowk made, as it is now: its common git directory's
        // parts a commit writes, the fences on top of them, and who the
        // commit is by.
        let (mut git, mut swept, mut identity) = (Vec::new(), Vec::new(), None);
        if let Some(w) = worktrees.as_deref().and_then(|r| managed_worktree(&writable[0], r)) {
            // A link a call left there (one that crashed, or ran before
            // the sweep after it) goes before this one binds anything.
            swept = w.swept();
            for (d, deep) in &swept {
                sweep(d, *deep, &mut Vec::new());
            }
            git = w.binds();
            read_only.extend(w.read_only(&writable[0], home));
            identity = git_identity(&writable[0], config_env);
        }
        // A settings directory outside the workspace is read-only already
        // (the root is bound read-only, the home is hidden); one inside it
        // is bound back read-only over the writable bind.
        read_only.extend(protected.iter().map(|d| canon(d)).filter(|d| writable.iter().any(|w| d.starts_with(w))));
        read_only.sort();
        read_only.dedup();
        let mut hidden: Vec<PathBuf> = home.map(|h| CREDENTIALS.iter().map(|c| canon(&h.join(c))).collect()).unwrap_or_default();
        let toolchains = toolchains.clone();
        // Cargo's registry credentials, in the cargo home in use and in the
        // default one (`credentials` is the older name of the same file).
        let cargo_homes: Vec<PathBuf> = toolchains.iter().filter(|(k, _)| *k == "CARGO_HOME").map(|(_, d)| d.clone()).chain(home.map(|h| canon(&h.join(".cargo")))).collect();
        hidden.extend(cargo_homes.iter().flat_map(|d| ["credentials.toml", "credentials"].map(|f| d.join(f))));
        hidden.extend(secrets.iter().map(|d| canon(d)));
        hidden.dedup();
        let profile = sandbox.profile;
        let home = home.map(canon);
        Plan {
            inputs: inputs.clone(),
            profile,
            kernel: sandbox.by == By::Bubblewrap,
            cwd: canon(cwd),
            writable,
            git,
            swept,
            identity,
            read_only,
            hidden,
            home,
            readable: readable.iter().map(|d| canon(d)).chain(toolchains.iter().map(|(_, d)| d.clone())).collect(),
            refused,
            repos,
            toolchains,
        }
    }

    /// Whether a read of `real` (a path as it leads) is one the sandbox
    /// would not let a command make: inside a hidden directory, or inside
    /// the home and outside every directory bound back into it.
    pub fn hides(&self, real: &Path) -> bool {
        if self.hidden.iter().any(|h| real.starts_with(h)) {
            return true;
        }
        match &self.home {
            Some(home) => real.starts_with(home) && !self.writable.iter().chain(&self.readable).any(|w| real.starts_with(w)),
            None => false,
        }
    }

    /// Whether `real` is inside what the plan keeps read-only in the
    /// workspace: a repository's `.git`, a hooks directory one names, a
    /// settings directory. The file tools fence the same list the sandbox
    /// binds, so a command and a tool agree.
    pub fn fences(&self, real: &Path) -> bool {
        self.read_only.iter().any(|r| real.starts_with(r))
    }

    /// bubblewrap's arguments for the plan, before `--` and the command.
    /// Order matters: a later mount shadows an earlier one, so the
    /// read-only fences come after the writable binds they sit in, and the
    /// hidden directories after both.
    pub fn bwrap_args(&self) -> Vec<String> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut a: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", "/run"].map(String::from).to_vec();
        if self.profile.network() {
            // The resolver `/etc/resolv.conf` leads to, where systemd or
            // NetworkManager keep it; nothing else of `/run` comes back.
            // `workspace` means the network is on.
            for r in ["/run/systemd/resolve", "/run/NetworkManager", "/run/resolvconf"] {
                a.extend(["--ro-bind-try".into(), r.into(), r.into()]);
            }
        } else if std::fs::symlink_metadata("/etc/resolv.conf").is_ok_and(|m| m.is_file()) {
            // No resolver at all: nss-resolve's socket stays in the empty
            // `/run`, and a resolv.conf that is a file is emptied (one that
            // leads into `/run` already leads nowhere).
            a.extend(["--ro-bind", "/dev/null", "/etc/resolv.conf"].map(String::from));
        }
        if let Some(home) = &self.home {
            a.extend(["--tmpfs".into(), s(home)]);
            for r in &self.readable {
                a.extend(["--ro-bind-try".into(), s(r), s(r)]);
            }
        }
        let bind = if self.profile.writes() { "--bind" } else { "--ro-bind" };
        for w in &self.writable {
            a.extend([bind.into(), s(w), s(w)]);
        }
        // `-try`: a repository never logging refs has no `logs`.
        for (g, writes) in &self.git {
            let bind = if *writes && self.profile.writes() { "--bind-try" } else { "--ro-bind-try" };
            a.extend([bind.into(), s(g), s(g)]);
        }
        // `-try`: a fence that does not exist is not there to protect (and
        // `.git` is a file in a worktree, bound read-only all the same).
        for r in &self.read_only {
            a.extend(["--ro-bind-try".into(), s(r), s(r)]);
        }
        for h in &self.hidden {
            if h.is_dir() {
                a.extend(["--tmpfs".into(), s(h)]);
            } else if h.exists() {
                a.extend(["--ro-bind".into(), "/dev/null".into(), s(h)]);
            }
        }
        if !self.profile.network() {
            a.push("--unshare-net".into());
        }
        // Its own pid namespace, with bubblewrap as its first process: when
        // the call is killed, everything the command started goes with it.
        // The command's environment is the allowlist `env` names, nothing
        // inherited: no provider key, token or agent socket reaches it.
        a.push("--clearenv".into());
        for (k, v) in env() {
            a.extend(["--setenv".into(), k, v]);
        }
        for (k, d) in &self.toolchains {
            a.extend(["--setenv".into(), (*k).into(), s(d)]);
        }
        if let Some((name, email)) = &self.identity {
            for who in ["AUTHOR", "COMMITTER"] {
                a.extend(["--setenv".into(), format!("GIT_{who}_NAME"), name.clone()]);
                a.extend(["--setenv".into(), format!("GIT_{who}_EMAIL"), email.clone()]);
            }
        }
        a.extend(["--dir".into(), HOME.into()]);
        // A session of its own, so it cannot push keystrokes into the
        // terminal krowk runs in (TIOCSTI); its own IPC, host name and
        // cgroup view.
        a.extend(["--new-session", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try"].map(String::from));
        a.extend(["--unshare-pid", "--die-with-parent", "--chdir"].map(String::from));
        a.push(s(&self.cwd));
        a
    }
}

/// The fences a call found missing, which bubblewrap cannot bind
/// read-only because there is nothing to bind: a `.git` or `.claude` the
/// command creates would hold hooks that run the next time git or an agent
/// starts there. Whatever of them the call left behind is removed when
/// this is dropped — after the call, when its pid namespace, and so
/// everything it started, is gone — and named by `appeared`.
/// A repository nested anywhere in the workspace that the call created —
/// `git init` in a subdirectory, a clone — is removed the same way: the
/// workspace is searched again after the call for every `.git` it did not
/// have before.
///
/// A `.git` is known by its device and inode, not its path: a directory
/// holding a repository that a command renames (`mv sub moved`) keeps its
/// `.git`'s inode and is left alone, while one made new (`git init`, a
/// clone, a copy) has an inode the workspace did not have, and goes.
pub struct Unfenced {
    missing: Vec<PathBuf>,
    swept: Vec<(PathBuf, bool)>,
    writable: Vec<PathBuf>,
    home: Option<PathBuf>,
    repos: Vec<(u64, u64)>,
    walk: Walk,
}

/// A path's device and inode, not following a symlink.
fn identity(p: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

impl Unfenced {
    pub fn before(plan: &Plan) -> Unfenced {
        let missing = plan.read_only.iter().filter(|p| std::fs::symlink_metadata(p).is_err()).cloned().collect();
        let repos = plan.repos.iter().filter_map(|r| identity(r)).collect();
        Unfenced { missing, swept: plan.swept.clone(), writable: plan.writable.clone(), home: plan.home.clone(), repos, walk: plan.inputs.walk.clone() }
    }

    /// Removes what appeared and says so; empty when nothing did.
    pub fn appeared(&mut self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (d, deep) in std::mem::take(&mut self.swept) {
            sweep(&d, deep, &mut out);
        }
        let mut gone = std::mem::take(&mut self.missing);
        if let Ok(now) = self.walk.lock().unwrap_or_else(|e| e.into_inner()).repositories(&std::mem::take(&mut self.writable), self.home.as_deref()) {
            gone.extend(now.repos.into_iter().filter(|r| identity(r).is_none_or(|id| !self.repos.contains(&id))));
        }
        for p in gone {
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            // Not followed: a symlink is removed as a link, and a directory's
            // contents are removed without following the links in it.
            let _ = if m.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            out.push(p);
        }
        out
    }
}

impl Drop for Unfenced {
    fn drop(&mut self) {
        self.appeared();
    }
}

/// Entries a workspace search looks at before it gives up — and refuses
/// the sandbox, since a repository it did not reach is one it cannot fence.
const WALK_BUDGET: usize = 400_000;

/// What the workspace search found: every `.git` (a directory, or a file
/// naming a git directory elsewhere), and each hooks directory a
/// repository's `core.hooksPath` names inside a writable root.
#[derive(Debug, Default)]
struct Fences {
    repos: Vec<PathBuf>,
    extra: Vec<PathBuf>,
}

impl Fences {
    fn all(self) -> impl Iterator<Item = PathBuf> {
        self.repos.into_iter().chain(self.extra)
    }
}

/// A turn's workspace search, shared by its calls: every directory it
/// read, known by its device, inode and change time, with what it held. A
/// later search stats each directory it knows and reads again only those
/// that changed — a file added, removed or renamed in a directory changes
/// its time — so an unchanged workspace costs a sweep of `stat`s,
/// and a `.git` made anywhere since is found. A repository's config (its
/// `core.hooksPath`) and a gitdir file are known the same way.
pub type Walk = std::sync::Arc<std::sync::Mutex<WalkCache>>;

#[derive(Debug, Default)]
pub struct WalkCache {
    dirs: std::collections::HashMap<PathBuf, Seen>,
    gits: std::collections::HashMap<PathBuf, (Stamp, Option<PathBuf>, Option<PathBuf>)>,
}

/// A directory or file as `stat` sees it, not following a symlink: its
/// device and inode, and its change time — which, unlike the modification
/// time, no command can set back (`touch -d` moves `mtime`, and moves
/// `ctime` forward in doing so).
type Stamp = (u64, u64, i64, i64);

fn stamp(p: &Path) -> Option<Stamp> {
    stamp_of(&std::fs::symlink_metadata(p).ok()?)
}

fn stamp_of(m: &std::fs::Metadata) -> Option<Stamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
    }
    #[cfg(not(unix))]
    {
        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some((0, 0, t.as_secs() as i64, t.subsec_nanos() as i64))
    }
}

/// What one directory held when it was read.
#[derive(Debug)]
struct Seen {
    stamp: Stamp,
    entries: usize,
    subdirs: Vec<PathBuf>,
    gits: Vec<(PathBuf, bool)>,
}

impl WalkCache {
    /// Every repository in each writable root, following no symlink and not
    /// descending into a `.git` (compared as the file tools compare it,
    /// case-folded, with Windows' trailing dots and spaces dropped).
    fn repositories(&mut self, roots: &[PathBuf], home: Option<&Path>) -> Result<Fences, String> {
        let is_git = |n: &std::ffi::OsStr| n.to_string_lossy().trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git");
        let within = |p: &Path| roots.iter().any(|r| p.starts_with(r));
        let mut f = Fences::default();
        let mut seen = 0usize;
        let mut visited = std::collections::HashSet::new();
        for root in roots {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let Some(st) = std::fs::symlink_metadata(&dir).ok().filter(std::fs::Metadata::is_dir).and_then(|m| stamp_of(&m)) else { continue };
                if !self.dirs.get(&dir).is_some_and(|s| s.stamp == st) {
                    let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                    let mut s = Seen { stamp: st, entries: 0, subdirs: Vec::new(), gits: Vec::new() };
                    for e in rd.flatten() {
                        s.entries += 1;
                        let Ok(t) = e.file_type() else { continue };
                        if is_git(&e.file_name()) {
                            s.gits.push((e.path(), t.is_dir()));
                        } else if t.is_dir() {
                            s.subdirs.push(e.path());
                        }
                    }
                    self.dirs.insert(dir.clone(), s);
                }
                let s = &self.dirs[&dir];
                seen += s.entries;
                if seen > WALK_BUDGET {
                    return Err(format!(
                        "the workspace holds more than {seen} files and directories, over the {WALK_BUDGET} the sandbox searches for repositories to fence, so it cannot fence them all — run krowk in a smaller directory, or pass --sandbox off to run without the sandbox"
                    ));
                }
                stack.extend(s.subdirs.iter().cloned());
                let gits = s.gits.clone();
                visited.insert(dir.clone());
                for (p, is_dir) in gits {
                    // A git directory's config, or a gitdir file, as it is now.
                    let watched = if is_dir { p.join("config") } else { p.clone() };
                    let st = stamp(&watched).unwrap_or_default();
                    let fresh = self.gits.get(&p).is_some_and(|(s, _, _)| *s == st);
                    if !fresh {
                        let repo = dir.clone();
                        let gitdir = if is_dir { Some(p.clone()) } else { git_file_target(&p, &repo) };
                        let hooks = gitdir.as_deref().and_then(|g| hooks_path(g, &repo, home));
                        self.gits.insert(p.clone(), (st, gitdir.filter(|g| g != &p), hooks));
                    }
                    let (_, gitdir, hooks) = &self.gits[&p];
                    f.extra.extend(gitdir.iter().chain(hooks).filter(|g| within(g)).cloned());
                    f.repos.push(p);
                }
            }
        }
        // What is no longer there is forgotten, so the cache stays the size
        // of the workspace.
        self.dirs.retain(|d, _| visited.contains(d));
        let repos: std::collections::HashSet<&PathBuf> = f.repos.iter().collect();
        self.gits.retain(|g, _| repos.contains(g));
        Ok(f)
    }
}

/// A small regular file's text, read without following a symlink and
/// without blocking: the workspace is the model's to write, and a FIFO
/// named `.git` or `.git/config` must not hang the search.
fn read_small(p: &Path) -> Option<String> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let f = o.open(p).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut s = String::new();
    f.take(1 << 20).read_to_string(&mut s).ok()?;
    Some(s)
}

/// Where a `.git` file (`gitdir: <path>`, a worktree's or a submodule's)
/// leads, resolved against the repository.
fn git_file_target(file: &Path, repo: &Path) -> Option<PathBuf> {
    let text = read_small(file)?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let p = if Path::new(target).is_absolute() { PathBuf::from(target) } else { repo.join(target) };
    Some(p.canonicalize().unwrap_or(p))
}

/// The hooks directory a git directory's config names (`core.hooksPath`),
/// read as text — git itself is not run in a repository a model may have
/// written — and resolved as git does: `~/` against the home, a relative
/// path against the repository's working tree.
fn hooks_path(gitdir: &Path, repo: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let text = read_small(&gitdir.join("config"))?;
    let mut core = false;
    let mut found = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            core = line.trim_start_matches('[').trim_end_matches(']').trim().eq_ignore_ascii_case("core");
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && core
            && k.trim().eq_ignore_ascii_case("hookspath")
        {
            found = Some(v.trim().trim_matches('"').to_string());
        }
    }
    let v = found.filter(|v| !v.is_empty())?;
    let p = match v.strip_prefix("~/") {
        Some(rest) => home?.join(rest),
        None if Path::new(&v).is_absolute() => PathBuf::from(&v),
        None => repo.join(&v),
    };
    Some(krowk_api::home::lexical(&p))
}

/// `krowk_api::home::worktrees_root`, read from `env`.
fn worktrees_root(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    krowk_api::home::worktrees_root(&|k| env(k).map(|v| v.to_string_lossy().into_owned()).unwrap_or_default())
}

/// A worktree krowk made for an agent, as the sandbox opens it to a
/// commit: the repository's common git directory and the worktree's admin
/// directory in it, both as they lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub common: PathBuf,
    pub admin: PathBuf,
    /// The `<hex>` of its `krowk/<hex>` branch.
    pub hex: String,
}

/// Whether `top`, a workspace, is a worktree krowk made under `root`
/// (`krowk_api::home::worktrees_root`), and where its git directories are.
/// The root is named by the environment, so being under it is not enough:
/// `top/.git` must be a gitdir file leading to `<common>/worktrees/<name>`,
/// that admin directory's `commondir` and `gitdir` must lead back to
/// `<common>` and to `top/.git`, `<common>` must be a git directory (its
/// `objects`, `refs` and `HEAD`) outside the root, and the worktree must
/// have a `krowk/<8 hex>` branch checked out. Read as text, as everything
/// else here is: git is not run in a directory a model may have written.
pub fn managed_worktree(top: &Path, root: &Path) -> Option<ManagedWorktree> {
    let (top, root) = (top.canonicalize().ok()?, root.canonicalize().ok()?);
    if top == root || !top.starts_with(&root) {
        return None;
    }
    let admin = git_file_target(&top.join(".git"), &top).filter(|a| a.is_dir())?;
    let parent = admin.parent().filter(|p| p.file_name().is_some_and(|n| n == "worktrees"))?;
    let common = parent.parent()?.canonicalize().ok()?;
    if common.starts_with(&root) || top.starts_with(&common) {
        return None;
    }
    let real = common.join("objects").is_dir() && common.join("refs").is_dir() && read_small(&common.join("HEAD")).is_some();
    let leads = |file: &str, to: &Path| {
        let text = read_small(&admin.join(file))?;
        let p = admin.join(text.trim());
        p.canonicalize().ok().filter(|p| p == to)
    };
    if !real || leads("commondir", &common).is_none() || leads("gitdir", &top.join(".git")).is_none() {
        return None;
    }
    let branch = branch_of(&admin)?;
    let hex = branch.strip_prefix("refs/heads/krowk/")?;
    (hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then(|| ManagedWorktree { common, admin, hex: hex.to_string() })
}

/// The branch a git directory has checked out (`ref: refs/heads/…` in its
/// `HEAD`), when it names one plainly: no `..`, nothing absolute.
fn branch_of(gitdir: &Path) -> Option<String> {
    let text = read_small(&gitdir.join("HEAD"))?;
    let r = text.trim().strip_prefix("ref:")?.trim();
    let plain = r.starts_with("refs/heads/") && Path::new(r).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    plain.then(|| r.to_string())
}

impl ManagedWorktree {
    /// What the sandbox binds of the common directory, in order: all of it
    /// read-only (it may be in the hidden home), then writable what a
    /// commit on `krowk/<hex>` writes — objects, the `krowk/` branches and
    /// their reflogs, and the worktree's own index, `HEAD` and logs in its
    /// admin directory. Not the rest of `logs`: git appends to a reflog
    /// outside the sandbox too, so one a command made a symlink would have
    /// the person's next commit append to whatever it leads to. The two
    /// `krowk` directories are made here when missing, and left read-only
    /// when one is not a directory of its own.
    fn binds(&self) -> Vec<(PathBuf, bool)> {
        let c = &self.common;
        let mut out = vec![(c.clone(), false), (c.join("objects"), true)];
        out.extend(["refs/heads/krowk", "logs/refs/heads/krowk"].into_iter().filter_map(|d| own_dir(c, d)).map(|d| (d, true)));
        out.push((self.admin.clone(), true));
        out
    }

    /// The writable directories `sweep` keeps to regular files and
    /// directories, and whether all of each: `binds`' whole, but of
    /// `objects` (thousands of files) only its top — the fan-out
    /// directories git writes new objects into — `pack` and `info`.
    fn swept(&self) -> Vec<(PathBuf, bool)> {
        let c = &self.common;
        let mut out: Vec<(PathBuf, bool)> = ["refs/heads/krowk", "logs/refs/heads/krowk"].iter().map(|d| (c.join(d), true)).collect();
        out.push((self.admin.clone(), true));
        out.extend(["objects", "objects/pack", "objects/info"].iter().map(|d| (c.join(d), false)));
        out
    }

    /// What stays read-only on top of `binds`, the worktree at `top`: in
    /// the writable directories, every other `krowk/` branch and what
    /// leads this worktree to its repository or runs in it (`commondir`,
    /// `gitdir`, `config.worktree`, a submodule's git directory); and,
    /// read-only already but fenced from the file tools too, what git runs
    /// and what another checkout is on. A missing one a command makes is
    /// removed after the call, as a missing `.git` is (`Unfenced`).
    fn read_only(&self, top: &Path, home: Option<&Path>) -> Vec<PathBuf> {
        let c = &self.common;
        let mut out: Vec<PathBuf> = ["config", "hooks", "info", "HEAD", "modules", "shallow", "packed-refs"].iter().map(|f| c.join(f)).collect();
        out.extend(hooks_path(c, top, home));
        out.extend(["config.worktree", "commondir", "gitdir", "modules"].iter().map(|f| self.admin.join(f)));
        out.push(top.join(".git"));
        let own = std::ffi::OsString::from(&self.hex);
        out.extend(std::fs::read_dir(c.join("refs/heads/krowk")).into_iter().flatten().flatten().filter(|e| e.file_name() != own && e.file_type().is_ok_and(|t| t.is_file() || t.is_dir())).map(|e| e.path()));
        out.extend(std::fs::read_dir(c.join("worktrees")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.file_name() != self.admin.file_name()));
        out
    }
}

/// `rel` under `base`, made when missing, when every part of it is a
/// directory and none a symlink: a bind follows a link, and a writable
/// one would then open wherever it leads.
fn own_dir(base: &Path, rel: &str) -> Option<PathBuf> {
    let d = base.join(rel);
    let _ = std::fs::create_dir_all(&d);
    let mut at = base.to_path_buf();
    for part in Path::new(rel).components() {
        at.push(part);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
            return None;
        }
    }
    Some(d)
}

/// Removes from `dir`, recursively when `deep` and following no link,
/// everything that is not a regular file or a directory — a symlink, a
/// FIFO, a socket — and names it in `out`. git outside the sandbox writes
/// through what is in a krowk worktree's writable git directories (it
/// appends to a reflog in place), so a link a command left there would
/// have it write wherever the link leads. Bounded, as the workspace
/// search is.
fn sweep(dir: &Path, deep: bool, out: &mut Vec<PathBuf>) {
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        if !std::fs::symlink_metadata(&d).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > WALK_BUDGET {
                return;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    if deep {
                        stack.push(e.path());
                    }
                }
                Ok(t) if t.is_file() => {}
                _ => {
                    if std::fs::remove_file(e.path()).is_ok() {
                        out.push(e.path());
                    }
                }
            }
        }
    }
}

/// `user.name` and `user.email` as git resolves them in `top`, with
/// `config_env` saying where the person's own config is; none unless both
/// are set. Read by git outside the sandbox, through krowk's git (hooks
/// and fsmonitor off): `git config` reads config and runs nothing.
fn git_identity(top: &Path, config_env: &[(&'static str, Option<std::ffi::OsString>)]) -> Option<(String, String)> {
    let mut c = krowk_api::git::query(top).ok()?;
    c.args(["config", "--get-regexp", r"^user\.(name|email)$"]).stderr(std::process::Stdio::null());
    for (k, v) in config_env {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    let out = c.output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    // The last of a key wins, as it does for git.
    let (mut name, mut email) = (None, None);
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("user.name", v)) => name = Some(v.to_string()),
            Some(("user.email", v)) => email = Some(v.to_string()),
            _ => {}
        }
    }
    Some((name.filter(|n| !n.is_empty())?, email.filter(|e| !e.is_empty())?))
}

/// The Rust toolchain's homes that exist: `RUSTUP_HOME` and `CARGO_HOME`
/// as krowk's environment names them, else `~/.rustup` and `~/.cargo`, as
/// they lead. rustup finds its toolchains only there, so a sandbox with a
/// private `HOME` and without them cannot run `cargo`.
fn toolchains(home: Option<&Path>, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Vec<(&'static str, PathBuf)> {
    [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")]
        .into_iter()
        .filter_map(|(k, d)| {
            let named = env(k).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home.map(|h| h.join(d)))?;
            named.canonicalize().ok().filter(|p| p.is_dir()).map(|p| (k, p))
        })
        .collect()
}

/// The environment a sandboxed command gets, and bubblewrap itself: the
/// search path, the terminal, the locale and the user's name from krowk's
/// own, the private home and `/tmp`. Everything else — `*_API_KEY`,
/// `*_TOKEN`, `KROWK_*`, `AWS_*`, `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` — is
/// left out, allowlisted rather than denylisted, so a variable krowk has
/// never heard of is left out too.
pub fn env() -> Vec<(String, String)> {
    let keep = |k: &str| matches!(k, "PATH" | "TERM" | "LANG" | "USER" | "LOGNAME") || k.starts_with("LC_");
    let mut out: Vec<(String, String)> = std::env::vars().filter(|(k, _)| keep(k)).collect();
    if !out.iter().any(|(k, _)| k == "PATH") {
        out.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    }
    out.push(("HOME".into(), HOME.into()));
    out.push(("TMPDIR".into(), "/tmp".into()));
    out.sort();
    out
}

/// The program that enforces a plan here, or why none can: the `fix` a
/// sandboxed run refuses with. Probed once per process.
pub fn enforcer() -> Result<&'static Path, String> {
    static PROBED: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    PROBED.get_or_init(probe).as_ref().map(PathBuf::as_path).map_err(Clone::clone)
}

#[cfg(target_os = "linux")]
fn probe() -> Result<PathBuf, String> {
    probe_in(&std::env::var("PATH").unwrap_or_default())
}

/// `probe`, finding `bwrap` on `path`.
#[cfg(target_os = "linux")]
fn probe_in(path: &str) -> Result<PathBuf, String> {
    let Some(bwrap) = crate::instances::find_binary("bwrap", path) else {
        return Err("the sandbox needs bubblewrap, which is not installed — install it (`apt install bubblewrap`, `dnf install bubblewrap`, `pacman -S bubblewrap`), or run without --sandbox".into());
    };
    // The flags every profile uses, so a kernel that forbids one (a user
    // or network namespace) is found here and not on the first call.
    let out = std::process::Command::new(&bwrap)
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--unshare-net", "--unshare-pid", "--die-with-parent", "--new-session", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try", "--clearenv", "--", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(bwrap),
        Ok(o) => Err(format!(
            "bubblewrap ({}) is installed but cannot make a sandbox here: {} — unprivileged user namespaces are off (on Ubuntu 24.04, AppArmor's `kernel.apparmor_restrict_unprivileged_userns`); allow them for bwrap, or run without --sandbox",
            bwrap.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("bubblewrap ({}) could not be run: {e} — reinstall it, or run without --sandbox", bwrap.display())),
    }
}

#[cfg(target_os = "macos")]
fn probe() -> Result<PathBuf, String> {
    Err("the macOS sandbox (Seatbelt) is not built yet, so a sandboxed run cannot start here — run without --sandbox, or on Linux with bubblewrap".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe() -> Result<PathBuf, String> {
    Err("krowk has no sandbox on this platform yet, so a sandboxed run cannot start here — run without --sandbox, or on Linux with bubblewrap".into())
}

/// The program and arguments that run `bash -c <command>` inside `plan`,
/// or why it cannot run at all.
pub fn bash(plan: &Plan, command: &str) -> Result<(PathBuf, Vec<String>), String> {
    let bwrap = enforcer()?;
    let mut args = plan.bwrap_args();
    args.extend(["--".into(), "bash".into(), "-c".into(), command.into()]);
    Ok((bwrap.to_path_buf(), args))
}

/// Whether krowk runs inside a container, which is a sandbox of its own:
/// Docker's and Podman's markers, which only the container's root writes.
pub fn in_container() -> bool {
    cfg!(target_os = "linux") && (Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(profile: Profile, base: &Path) -> Plan {
        let home = base.join("home");
        Plan::new(Sandbox { profile, by: By::Bubblewrap }, &base.join("ws"), &[], &[], &[home.join(".claude")], &[home.join(".krowk")], Some(&home))
    }

    /// R-PERM-3: `.git` and its kind, the settings directories and the
    /// credential directories are laid out before anything runs, and the
    /// fences come after the writable bind they sit in.
    #[test]
    fn r_perm_3_the_plan_fences_git_and_settings_and_hides_credentials() {
        let base = crate::tools::tests::dir("sandbox-plan");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(base.join("ws")).unwrap();
        std::fs::create_dir_all(base.join("home/.ssh")).unwrap();
        std::fs::create_dir_all(base.join("home/src")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("home/.ssh"), base.join("home/keys")).unwrap();
        // A nested repository naming its own hooks directory, and one that
        // is a gitdir file (a worktree's, a submodule's).
        std::fs::create_dir_all(base.join("ws/sub/.git")).unwrap();
        std::fs::write(base.join("ws/sub/.git/config"), "[core]\n\tbare = false\n[Core]\n\thooksPath = \"tools/hooks\"\n").unwrap();
        std::fs::create_dir_all(base.join("ws/wt")).unwrap();
        std::fs::write(base.join("ws/wt/.git"), "gitdir: ../sub/.git\n").unwrap();
        let base = base.canonicalize().unwrap();
        let p = plan(Profile::Workspace, &base);
        for fenced in ["ws/sub/.git", "ws/sub/tools/hooks", "ws/wt/.git"] {
            assert!(p.fences(&base.join(fenced).join("x")), "{fenced}: {:?}", p.read_only);
        }
        assert_eq!(p.writable, [base.join("ws")]);
        for d in [".git", ".claude", ".codex", ".krowk"] {
            assert!(p.read_only.contains(&base.join("ws").join(d)), "{d}");
        }
        assert!(p.hides(&base.join("home/.claude/settings.json")) && !p.fences(&base.join("home/.claude")), "outside the workspace a settings directory is hidden with the home, not bound back");
        assert!(p.hidden.contains(&base.join("home/.ssh")) && p.hidden.contains(&base.join("home/.krowk")));
        let args = p.bwrap_args();
        let at = |x: &str, v: &Path| args.windows(3).position(|w| w[0] == x && w[1] == v.to_string_lossy()).unwrap_or_else(|| panic!("{x} {}", v.display()));
        assert!(at("--bind", &base.join("ws")) < at("--ro-bind-try", &base.join("ws/.git")));
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == base.join("home/.ssh").to_string_lossy()));
        assert!(!args.contains(&"--unshare-net".to_string()), "workspace keeps the network");
        assert!(args.contains(&"/run/systemd/resolve".to_string()), "and its resolver");
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == base.join("home").to_string_lossy()), "the home is hidden");
        assert!(!args.iter().any(|a| a.starts_with(&*base.join("home/src").to_string_lossy()) || a.starts_with(&*base.join("home/keys").to_string_lossy())), "and nothing of it comes back");
        assert!(p.hides(&base.join("home/.ssh/id_ed25519")) && p.hides(&base.join("home/src/notes.txt")), "every profile hides the whole home");
        let strict = plan(Profile::Strict, &base);
        assert!(strict.bwrap_args().contains(&"--unshare-net".to_string()));
        assert!(!strict.bwrap_args().iter().any(|a| a.starts_with("/run/")), "strict mounts no resolver");
        assert!(strict.hides(&base.join("home/src")) && !strict.hides(&base.join("ws/a.rs")), "strict hides the home outside the workspace");
        let ro = plan(Profile::ReadOnly, &base);
        assert!(ro.bwrap_args().windows(2).any(|w| w[0] == "--ro-bind" && w[1] == base.join("ws").to_string_lossy()), "read-only binds the workspace read-only");
        assert!(Profile::NAMES.iter().all(|n| Profile::parse(n).map(Profile::name) == Some(n)));
    }

    /// The workspace search's cost, first and cached, over trees of 60k
    /// and 400k entries (100 files a directory): run by hand with
    /// `--ignored --nocapture`, for the numbers the PR quotes.
    #[test]
    #[ignore]
    fn workspace_search_cost() {
        for total in [60_000usize, 399_000] {
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/tmp/walk-cost-{total}"));
            if !base.exists() {
                for d in 0..total / 101 {
                    let dir = base.join(format!("d{:03}/e{d}", d % 100));
                    std::fs::create_dir_all(&dir).unwrap();
                    for f in 0..100 {
                        std::fs::write(dir.join(format!("f{f}")), "").unwrap();
                    }
                }
            }
            let base = base.canonicalize().unwrap();
            let mut cache = WalkCache::default();
            let t = std::time::Instant::now();
            cache.repositories(std::slice::from_ref(&base), None).unwrap();
            let first = t.elapsed();
            let t = std::time::Instant::now();
            cache.repositories(std::slice::from_ref(&base), None).unwrap();
            let cached = t.elapsed();
            eprintln!("walk {total} entries ({} directories): first {first:?}, cached {cached:?}", cache.dirs.len());
        }
    }

    /// R-PERM-3: on a system without bubblewrap, a sandboxed run refuses
    /// with a fix naming it, rather than running unsandboxed.
    #[cfg(target_os = "linux")]
    #[test]
    fn r_perm_3_without_bubblewrap_a_sandboxed_run_refuses_with_a_fix() {
        let e = probe_in("/nonexistent").unwrap_err();
        assert!(e.contains("bubblewrap") && e.contains("install"), "{e}");
    }

    /// R-PERM-3: without an enforcer, a sandboxed command refuses with a
    /// fix rather than running unsandboxed.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn r_perm_3_a_platform_without_a_sandbox_refuses_with_a_fix() {
        let base = crate::tools::tests::dir("sandbox-none");
        let e = bash(&plan(Profile::Workspace, &base), "true").unwrap_err();
        assert!(e.contains("not built yet") || e.contains("no sandbox"), "{e}");
    }
}
