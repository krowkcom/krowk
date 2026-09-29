//! The OS sandbox (R-PERM-3): the boundary a permission prompt is not.
//! A command the `bash` tool runs is started inside it, so what the
//! command does — and whatever it starts — meets the kernel's refusal,
//! not krowk's judgement of a command line it cannot fully read.
//!
//! Three named profiles:
//!
//! - **workspace**: the working directory and the directories the
//!   settings add are writable; the rest of the file system is read-only;
//!   credential directories (`~/.ssh`, `~/.gnupg`, `~/.aws`, …) and
//!   krowk's home are hidden; the network is open.
//! - **read-only**: nothing is writable but a private `/tmp`; the same
//!   directories are hidden; no network.
//! - **strict**: the workspace is writable, the person's whole home
//!   outside it is hidden, and there is no network.
//!
//! In every profile `.git`, `.claude`, `.codex` and `.krowk` inside a
//! writable directory stay read-only, and so do the directories whose
//! settings and hooks decide what runs next (Claude Code's, krowk's own):
//! a command that could write `.git/hooks/pre-commit` or
//! `.claude/settings.json` could run anything the next time git or an
//! agent starts. `/tmp` is private to the call, and `/run` — where the
//! ssh agent, the session bus and the Docker socket live — is empty but
//! for the resolver. The Rust toolchain's homes are readable and named to
//! the command, cargo's registry credentials in them hidden; they are not
//! writable, so a build of what is already fetched works and a fetch does
//! not — the simpler of the safe choices. The command's environment is an allowlist (`env`),
//! it inherits no descriptor past stdio, and it runs in a session, IPC,
//! UTS and pid namespace of its own.
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
//! `tools::Scope::path`), and no rule, grant, person or mode opens them.

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub profile: Profile,
    /// Whether commands run inside bubblewrap; false inside a container,
    /// where only the file tools' own fences are krowk's to add.
    pub kernel: bool,
    pub cwd: PathBuf,
    /// Bound read-write (read-only under the read-only profile).
    pub writable: Vec<PathBuf>,
    /// Bound read-only over what `writable` opened: `.git` and its kind,
    /// and the settings directories that decide what runs.
    pub read_only: Vec<PathBuf>,
    /// Replaced by an empty directory.
    pub hidden: Vec<PathBuf>,
    /// Under strict, the home replaced by an empty one before the
    /// workspace is bound back into it.
    pub home: Option<PathBuf>,
    /// Directories bound read-only back into a hidden home: the skills.
    pub readable: Vec<PathBuf>,
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

    /// `new`, reading `RUSTUP_HOME` and `CARGO_HOME` from `env`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_in(sandbox: Sandbox, cwd: &Path, roots: &[PathBuf], readable: &[PathBuf], protected: &[PathBuf], secrets: &[PathBuf], home: Option<&Path>, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Plan {
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
        read_only.extend(protected.iter().map(|d| canon(d)));
        let mut hidden: Vec<PathBuf> = home.map(|h| CREDENTIALS.iter().map(|c| canon(&h.join(c))).collect()).unwrap_or_default();
        let toolchains = toolchains(home, env);
        // `credentials` is cargo's older name for the same file.
        hidden.extend(toolchains.iter().filter(|(k, _)| *k == "CARGO_HOME").flat_map(|(_, d)| ["credentials.toml", "credentials"].map(|f| d.join(f))));
        hidden.extend(secrets.iter().map(|d| canon(d)));
        hidden.dedup();
        let profile = sandbox.profile;
        Plan {
            profile,
            kernel: sandbox.by == By::Bubblewrap,
            cwd: canon(cwd),
            writable,
            read_only,
            hidden,
            home: home.filter(|_| profile == Profile::Strict).map(canon),
            readable: readable.iter().map(|d| canon(d)).chain(toolchains.iter().map(|(_, d)| d.clone())).collect(),
            toolchains,
        }
    }

    /// Whether a read of `real` (a path as it leads) is one the sandbox
    /// would not let a command make: inside a hidden directory, or under
    /// strict inside the home and outside every directory bound back.
    pub fn hides(&self, real: &Path) -> bool {
        if self.hidden.iter().any(|h| real.starts_with(h)) {
            return true;
        }
        match &self.home {
            Some(home) => real.starts_with(home) && !self.writable.iter().chain(&self.readable).any(|w| real.starts_with(w)),
            None => false,
        }
    }

    /// bubblewrap's arguments for the plan, before `--` and the command.
    /// Order matters: a later mount shadows an earlier one, so the
    /// read-only fences come after the writable binds they sit in, and the
    /// hidden directories after both.
    pub fn bwrap_args(&self) -> Vec<String> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut a: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", "/run"].map(String::from).to_vec();
        // The resolver `/etc/resolv.conf` leads to, where systemd or
        // NetworkManager keep it; nothing else of `/run` comes back.
        for r in ["/run/systemd/resolve", "/run/NetworkManager", "/run/resolvconf"] {
            a.extend(["--ro-bind-try".into(), r.into(), r.into()]);
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
pub struct Unfenced(Vec<PathBuf>);

impl Unfenced {
    pub fn before(plan: &Plan) -> Unfenced {
        Unfenced(plan.read_only.iter().filter(|p| std::fs::symlink_metadata(p).is_err()).cloned().collect())
    }

    /// Removes what appeared and says so; empty when nothing did.
    pub fn appeared(&mut self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for p in std::mem::take(&mut self.0) {
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
        std::fs::create_dir_all(base.join("ws")).unwrap();
        std::fs::create_dir_all(base.join("home/.ssh")).unwrap();
        let base = base.canonicalize().unwrap();
        let p = plan(Profile::Workspace, &base);
        assert_eq!(p.writable, [base.join("ws")]);
        for d in [".git", ".claude", ".codex", ".krowk"] {
            assert!(p.read_only.contains(&base.join("ws").join(d)), "{d}");
        }
        assert!(p.read_only.contains(&base.join("home/.claude")));
        assert!(p.hidden.contains(&base.join("home/.ssh")) && p.hidden.contains(&base.join("home/.krowk")));
        let args = p.bwrap_args();
        let at = |x: &str, v: &Path| args.windows(3).position(|w| w[0] == x && w[1] == v.to_string_lossy()).unwrap_or_else(|| panic!("{x} {}", v.display()));
        assert!(at("--bind", &base.join("ws")) < at("--ro-bind-try", &base.join("ws/.git")));
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == base.join("home/.ssh").to_string_lossy()));
        assert!(!args.contains(&"--unshare-net".to_string()), "workspace keeps the network");
        assert!(p.hides(&base.join("home/.ssh/id_ed25519")) && !p.hides(&base.join("home/src")));
        let strict = plan(Profile::Strict, &base);
        assert!(strict.bwrap_args().contains(&"--unshare-net".to_string()));
        assert!(strict.hides(&base.join("home/src")) && !strict.hides(&base.join("ws/a.rs")), "strict hides the home outside the workspace");
        let ro = plan(Profile::ReadOnly, &base);
        assert!(ro.bwrap_args().windows(2).any(|w| w[0] == "--ro-bind" && w[1] == base.join("ws").to_string_lossy()), "read-only binds the workspace read-only");
        assert!(Profile::NAMES.iter().all(|n| Profile::parse(n).map(Profile::name) == Some(n)));
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
