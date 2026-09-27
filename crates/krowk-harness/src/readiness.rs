//! Whether an instance can run a turn here, asked one way everywhere:
//! `krowk status`, `krowk doctor`, `krowk providers list`, and the host
//! before it moves a session or starts a turn (`check_model`, `settle`,
//! rollover candidates). Before this, `providers list` spawned each vendor
//! CLI in turn, the host checked only keys and binaries, and a SuperGrok
//! login counted as there whenever its name was in the credentials file.
//!
//! The answer is a `Readiness` and, around it, a `Report`: where the
//! credential comes from (`source` — an environment variable's name, a
//! vendor's own login, krowk's OAuth file; never the secret) and one line
//! that fixes it. A key is checked in the environment krowk resolved at
//! start, an OAuth login in krowk's provider credentials file (an expired
//! access token with no refresh token is `Expired`: nothing krowk can do
//! renews it), and a vendor login by asking the vendor — `claude auth status
//! --json`, Codex's `account/read` over `codex app-server` with `codex login
//! status` as the fallback — never by reading its files (R-BACK-2, R-BACK-3).
//!
//! **Where a vendor is asked matters.** Claude Code reads a project's
//! `.claude/settings.json` from its working directory, and a project can
//! make the login moot there (Bedrock or Vertex in its `env`, an
//! `apiKeyHelper`); Codex reads a project's `.codex/config.toml` the same
//! way (a `model_provider` that needs no OpenAI login). So a `Probe` names
//! the directory: before a turn, the session's own working directory,
//! once its repository is trusted — where the turn starts the vendor, and
//! so the answer the turn will get (Claude Code reads the settings of that
//! directory alone, not its parents') — and for `krowk status`, `providers
//! list`, `doctor` and a rollover offer, where no repository has been
//! trusted, krowk's own `0700` directory (`neutral_dir`), which holds
//! nothing and which nobody else can write into — never the shared
//! temporary directory, where anyone could plant a `.claude/settings.json`.
//!
//! Vendor checks spawn a process, so `check_all` runs them in parallel,
//! each bounded by one deadline (`VENDOR_TIMEOUT`) and killed with its
//! whole process group when it passes (unix; on Windows only the process
//! krowk started, see `probing`), and a long-lived host (the TUI) does
//! not re-ask for every switch: a vendor's "signed in" is kept for
//! `CACHE_FOR`. Only "signed in" is kept. A person who is told to sign in
//! does so in another terminal and tries again at once, and a remembered
//! "not signed in" would refuse them for a minute for nothing.

use crate::claude::auth as claude_auth;
use crate::codex::auth as codex_auth;
use crate::engine::EngineError;
use crate::instances::{Auth, Backend, Resolved};
use crate::oauth;
use crate::protocol::WireApi;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one vendor check may take before its answer is `Unknown`. Long
/// enough for a cold `claude` (a Node start) on a slow disk; checks run in
/// parallel, so a listing waits for the slowest one, not their sum.
pub const VENDOR_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a vendor's "signed in" is believed without asking again.
pub const CACHE_FOR: Duration = Duration::from_secs(60);
/// krowk's own directory for vendor checks outside any repository, under
/// its data directory.
pub const NEUTRAL_DIR: &str = "readiness";

/// Where a vendor is asked, and how long it has to answer — one deadline
/// for the whole check, a fallback included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub dir: PathBuf,
    pub within: Duration,
}

impl Probe {
    /// In `dir`, with `VENDOR_TIMEOUT`.
    pub fn at(dir: impl Into<PathBuf>) -> Probe {
        Probe { dir: dir.into(), within: VENDOR_TIMEOUT }
    }
}

/// krowk's own directory for asking a vendor outside any repository:
/// `<data dir>/readiness`, made `0700` and kept that way, and refused when
/// it is anything but a directory of its own (a symlink planted there
/// would lead the check somewhere else). It holds nothing, so a vendor
/// started in it reads only the person's own settings.
pub fn neutral_dir(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join(NEUTRAL_DIR);
    let fail = |e: std::io::Error| format!("{} cannot be made krowk's own: {e}", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(fail)?,
            Err(e) => return Err(fail(e)),
            Ok(m) if !m.is_dir() => return Err(format!("{} is not a directory — move it aside", dir.display())),
            // SAFETY: getuid has no preconditions and cannot fail.
            Ok(m) if m.uid() != unsafe { libc::getuid() } => return Err(format!("{} belongs to another user — move it aside", dir.display())),
            Ok(_) => {}
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir).map_err(fail)?;
    Ok(dir)
}

/// Whether an instance can run a turn, and if not, which kind of not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// It can. `source` names where the credential comes from.
    Ready { source: String },
    /// An API key is read from `var`, and `var` is not set.
    KeyNotSet { var: String },
    /// No login: the vendor says so, or krowk's OAuth file has none.
    NotSignedIn,
    /// A login whose access token has expired and cannot be refreshed.
    Expired,
    /// The vendor binary is not there.
    NotInstalled,
    /// The check itself failed: a vendor that did not answer in time, or
    /// answered in a way krowk does not read. Not a refusal: the turn is
    /// left to fail, or not, on its own.
    Unknown { reason: String },
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Readiness::Ready { .. })
    }

    /// The `state` field of `krowk status --json`: stable, snake_case.
    pub fn state(&self) -> &'static str {
        match self {
            Readiness::Ready { .. } => "ready",
            Readiness::KeyNotSet { .. } => "key_not_set",
            Readiness::NotSignedIn => "not_signed_in",
            Readiness::Expired => "expired",
            Readiness::NotInstalled => "not_installed",
            Readiness::Unknown { .. } => "unknown",
        }
    }

    /// The same, in words, for a table.
    pub fn label(&self) -> &'static str {
        match self {
            Readiness::Ready { .. } => "ready",
            Readiness::KeyNotSet { .. } => "key not set",
            Readiness::NotSignedIn => "not signed in",
            Readiness::Expired => "expired",
            Readiness::NotInstalled => "not installed",
            Readiness::Unknown { .. } => "unknown",
        }
    }
}

/// One instance's row: what `krowk status` prints, and what a refusal is
/// made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub instance: String,
    pub kind: &'static str,
    pub readiness: Readiness,
    /// Where the credential comes from — or, when it is not there, would.
    /// A name or a place, never a secret.
    pub source: String,
    /// What makes it ready; none when it is.
    pub fix: Option<String>,
}

impl Report {
    /// A row of `krowk status --json` (and `providers list`, `doctor`).
    /// Every key is always there, `null` when it has no value, so a script
    /// reads the same shape whatever the state.
    pub fn json(&self) -> Value {
        json!({
            "instance": self.instance,
            "kind": self.kind,
            "state": self.readiness.state(),
            "ready": self.readiness.is_ready(),
            "source": self.source,
            "fix": self.fix,
            "var": match &self.readiness { Readiness::KeyNotSet { var } => Some(var), _ => None },
            "reason": match &self.readiness { Readiness::Unknown { reason } => Some(reason), _ => None },
        })
    }

    /// Why a turn cannot run here, with its fix, before a session is moved
    /// or a process started — or none: ready, or a check that could not
    /// tell, which is left to the turn.
    pub fn refusal(&self, inst: &Resolved) -> Option<EngineError> {
        let name = &self.instance;
        let fix = self.fix.clone().unwrap_or_default();
        Some(match &self.readiness {
            Readiness::Ready { .. } | Readiness::Unknown { .. } => return None,
            Readiness::KeyNotSet { .. } => EngineError::new("not_authenticated", inst.missing_key().unwrap_or(fix)),
            Readiness::NotInstalled => EngineError::new("backend_not_found", format!("{} was not found — {fix}", inst.backend.as_ref().map(|b| b.binary.as_str()).unwrap_or_default())),
            Readiness::NotSignedIn => match inst.auth {
                Auth::OAuth { .. } => EngineError::new("not_authenticated", format!("the {name} instance is not signed in — {fix} (a SuperGrok or X Premium subscription)")),
                _ => EngineError::new("not_authenticated", format!("{} is not signed in for the {name} instance — {fix}", inst.vendor)).with_status(401),
            },
            Readiness::Expired => EngineError::new("not_authenticated", format!("the {name} instance's login has expired and cannot be refreshed — {fix}")).with_status(401),
        })
    }
}

impl Resolved {
    /// Why a call cannot be made, before one is: an API-key instance whose
    /// variable is unset. The readiness check is its one caller outside the
    /// wire clients, which keep it as a defensive check of their own.
    pub fn missing_key(&self) -> Option<String> {
        let keyed = self.auth == Auth::ApiKey || (self.auth == Auth::Vendor && !self.api_key_env.is_empty());
        (keyed && self.api_key.is_empty()).then(|| {
            format!("no API key for the {} instance — set {} (krowk reads the key from the environment, never from a file)", self.name, self.api_key_env)
        })
    }
}

/// What can be known without spawning anything: a key, an OAuth login, a
/// binary. None when only the vendor can say — a backend on its own login.
pub fn local(inst: &Resolved, credentials: &Path) -> Option<Readiness> {
    if let Some(b) = &inst.backend
        && b.path.is_none()
    {
        return Some(Readiness::NotInstalled);
    }
    if inst.missing_key().is_some() {
        return Some(Readiness::KeyNotSet { var: inst.api_key_env.clone() });
    }
    match &inst.auth {
        Auth::ApiKey => Some(Readiness::Ready { source: format!("${}", inst.api_key_env) }),
        Auth::Keyless => Some(Readiness::Ready { source: "no key".into() }),
        Auth::OAuth { .. } => Some(oauth_login(inst, credentials)),
        // A keyed backend runs on its key, which is krowk's to check, not a
        // login the vendor holds.
        Auth::Vendor if !inst.api_key_env.is_empty() => Some(Readiness::Ready { source: format!("${}, handed to {}", inst.api_key_env, inst.vendor) }),
        Auth::Vendor => None,
    }
}

/// An OAuth login, as krowk's own credentials file holds it. Nothing of
/// the tokens leaves this function but whether they can still be used.
fn oauth_login(inst: &Resolved, credentials: &Path) -> Readiness {
    match oauth::Store::new(credentials.to_path_buf()).load(&inst.name) {
        Err(e) => Readiness::Unknown { reason: e.message },
        Ok(None) => Readiness::NotSignedIn,
        Ok(Some(s)) if s.expired_for_good(krowk_store::now_ms()) => Readiness::Expired,
        Ok(Some(_)) => Readiness::Ready { source: format!("OAuth login in {}", credentials.display()) },
    }
}

/// One instance, asked: locally when that answers, else of its vendor
/// where `probe` says (bounded by its deadline, a "signed in" cached for
/// `CACHE_FOR`). Blocks for as long as the vendor takes.
pub fn check(inst: &Resolved, credentials: &Path, probe: &Probe) -> Report {
    let readiness = local(inst, credentials).unwrap_or_else(|| match &inst.backend {
        Some(b) => vendor_cached(inst, b, probe),
        None => Readiness::Unknown { reason: "no way to check this instance".into() },
    });
    report(inst, readiness, credentials)
}

/// Every instance, the vendor checks in parallel: as long as the slowest
/// one, never their sum. In the order given.
pub fn check_all(instances: &[&Resolved], credentials: &Path, probe: &Probe) -> Vec<Report> {
    std::thread::scope(|s| {
        let running: Vec<_> = instances.iter().map(|inst| s.spawn(move || check(inst, credentials, probe))).collect();
        running
            .into_iter()
            .zip(instances)
            .map(|(h, inst)| h.join().unwrap_or_else(|_| report(inst, Readiness::Unknown { reason: "the check panicked".into() }, credentials)))
            .collect()
    })
}

/// `check`, off an async runtime's thread: a vendor check blocks on a
/// process for up to `VENDOR_TIMEOUT`, and the host's runtime has one
/// thread that the TUI's drawing and every other session also run on.
pub async fn check_async(inst: &Resolved, credentials: &Path, probe: &Probe) -> Report {
    if let Some(r) = local(inst, credentials) {
        return report(inst, r, credentials);
    }
    let (inst2, creds, probe) = (inst.clone(), credentials.to_path_buf(), probe.clone());
    match tokio::task::spawn_blocking(move || check(&inst2, &creds, &probe)).await {
        Ok(r) => r,
        Err(_) => report(inst, Readiness::Unknown { reason: "the check did not finish".into() }, credentials),
    }
}

/// A readiness with its source and fix around it: the row every caller
/// prints or refuses with.
pub fn report(inst: &Resolved, readiness: Readiness, credentials: &Path) -> Report {
    let source = match &readiness {
        Readiness::Ready { source } => source.clone(),
        _ => expected_source(inst, credentials),
    };
    let fix = fix(inst, &readiness);
    Report { instance: inst.name.clone(), kind: inst.kind, readiness, source, fix }
}

/// Where the credential would come from, said of an instance that is not
/// ready, so the person knows which variable or login to look at.
fn expected_source(inst: &Resolved, credentials: &Path) -> String {
    match &inst.auth {
        Auth::ApiKey => format!("${}", inst.api_key_env),
        Auth::Keyless => "no key".into(),
        Auth::OAuth { .. } => format!("OAuth login in {}", credentials.display()),
        Auth::Vendor if !inst.api_key_env.is_empty() => format!("${}, handed to {}", inst.api_key_env, inst.vendor),
        Auth::Vendor => vendor_login_source(inst, None),
    }
}

/// "Claude Code's own login in ~/.claude", with what it said when it is one.
fn vendor_login_source(inst: &Resolved, said: Option<&str>) -> String {
    let at = inst.backend.as_ref().and_then(|b| b.home.as_ref()).map(|h| format!(" in {}", h.display())).unwrap_or_default();
    match said {
        Some(s) => format!("{}'s own login{at} ({s})", inst.vendor),
        None => format!("{}'s own login{at}", inst.vendor),
    }
}

/// `krowk providers add <vendor>[ --name N]` for a backend instance: the
/// command that runs the vendor's own login in the instance's directory.
fn add_command(inst: &Resolved) -> String {
    let vendor = match inst.wire_api {
        WireApi::CodexAppServer => "codex",
        _ => "claude",
    };
    match inst.name.split_once(':') {
        Some((_, n)) => format!("krowk providers add {vendor} --name {n}"),
        None if inst.name == vendor => format!("krowk providers add {vendor}"),
        None => format!("krowk providers add {vendor} --name {}", inst.name),
    }
}

fn fix(inst: &Resolved, r: &Readiness) -> Option<String> {
    let codex = inst.wire_api == WireApi::CodexAppServer;
    Some(match r {
        Readiness::Ready { .. } => return None,
        Readiness::KeyNotSet { var } => format!("set {var} (krowk reads the key from the environment, never from a file)"),
        Readiness::NotSignedIn | Readiness::Expired if matches!(inst.auth, Auth::OAuth { .. }) => format!("sign in with `{}`", oauth::login_command(&inst.name)),
        Readiness::NotSignedIn | Readiness::Expired => {
            let own = if codex { "Codex's" } else { "Claude's" };
            format!("sign in with `{}`, which runs {own} own login", add_command(inst))
        }
        Readiness::NotInstalled if codex => "install Codex (https://developers.openai.com/codex), or name the binary with `krowk providers add codex --binary <path>`".into(),
        Readiness::NotInstalled => "install Claude Code (https://claude.com/claude-code), or name the binary with `krowk providers add claude --binary <path>`".into(),
        Readiness::Unknown { .. } => "see the reason, then run `krowk status` again".into(),
    })
}

/// Vendor answers that said "signed in", by instance, and when.
static SIGNED_IN: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);

/// What makes two vendor checks the same question: the instance, the
/// directory it is asked in (a project's settings can change the answer),
/// and everything that changes which login the vendor reads. No key value
/// is in it — a keyed backend never reaches the vendor check.
fn cache_key(inst: &Resolved, b: &Backend, probe: &Probe) -> String {
    format!("{}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}", inst.name, probe.dir, b.path, b.config_dir, b.env, b.args)
}

fn vendor_cached(inst: &Resolved, b: &Backend, probe: &Probe) -> Readiness {
    let key = cache_key(inst, b, probe);
    let known = SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)).filter(|(at, _)| at.elapsed() < CACHE_FOR).map(|(_, s)| s.clone());
    if let Some(source) = known {
        return Readiness::Ready { source };
    }
    let r = vendor(inst, b, probe);
    if let Readiness::Ready { source } = &r {
        SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, (Instant::now(), source.clone()));
    }
    r
}

/// Asks the vendor. Only whether there is a login and its kind come back;
/// an email, a plan's owner, the part of a key Codex prints, stay out.
fn vendor(inst: &Resolved, b: &Backend, probe: &Probe) -> Readiness {
    let said = match inst.wire_api {
        WireApi::CodexAppServer => codex_auth::signed_in(b, probe).map(|st| (st.logged_in, st.describe())),
        _ => claude_auth::status(b, probe).map(|st| (st.logged_in, st.describe())),
    };
    match said {
        Ok((true, how)) => Readiness::Ready { source: vendor_login_source(inst, Some(&how)) },
        Ok((false, _)) => Readiness::NotSignedIn,
        Err(reason) => Readiness::Unknown { reason },
    }
}

/// A vendor process for a check: started in `probe.dir` — never wherever
/// krowk happens to run, whose project settings nobody may have trusted —
/// in a process group of its own, so the whole of it can be stopped. The
/// group is unix's: on Windows only the process krowk started is stopped,
/// and one it started in turn (`claude.cmd`'s Node) can outlive the check
/// until it exits by itself — a Job object would close that, and is not
/// worth a new dependency for a status check.
pub(crate) fn probing(cmd: &mut Command, probe: &Probe) -> std::io::Result<std::process::Child> {
    cmd.current_dir(&probe.dir);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    cmd.spawn()
}

/// Stops a check's process and everything it started — a Node runtime's
/// workers, a shell's `sleep` — and reaps it: nothing outlives a check
/// (on unix; see `probing`).
pub(crate) fn stop(child: &mut std::process::Child) {
    kill_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL to the process group `child` leads (`probing` made it its own).
/// Also after the leader exited: whatever it left behind in its group
/// would otherwise hold the output pipes, and the check with them.
fn kill_group(child: &std::process::Child) {
    #[cfg(unix)]
    // SAFETY: kill with a negative pid signals a process group; it touches
    // no memory. A group with nobody left in it is ESRCH, ignored.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Runs a check's command to its end, or stops it at `probe.within`: none
/// then. Its output is read while it runs, so a chatty vendor cannot fill a
/// pipe and hang on it, and read only until the deadline even once it has
/// exited: a process it left behind that escaped its group (`setsid`, a
/// daemonizing credential helper) can hold the pipes open for good, and is
/// then abandoned with whatever had been read — the check never waits on
/// it past the deadline.
pub(crate) fn output_within(cmd: &mut Command, probe: &Probe) -> std::io::Result<Option<Output>> {
    use std::io::Read;
    use std::sync::{mpsc, Arc};
    let within = probe.within;
    let mut child = probing(cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()), probe)?;
    // Each pipe is read into a buffer shared with this thread, and says
    // when it reached its end.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel::<()>();
        let into = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = p.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    into.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
                }
            }
            let _ = done.send(());
        });
        (buf, finished)
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    loop {
        let gone = match exited(&mut child) {
            Ok(gone) => gone,
            // Stopped and reaped before the error goes up, so nothing is
            // left running or unreaped (ECHILD when SIGCHLD is ignored).
            Err(e) => {
                stop(&mut child);
                return Err(e);
            }
        };
        if gone {
            // The vendor answered and left, not yet reaped, so its pid —
            // the group's id — is still its own: whatever it left running
            // in the group, holding the pipes the answer is read from, is
            // stopped before the pid can go to anyone else.
            kill_group(&child);
            break;
        }
        if started.elapsed() > within {
            stop(&mut child);
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.wait()?;
    let take = |(buf, finished): (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>)| {
        // A small floor: a vendor that answered right at the deadline
        // still has its last bytes copied; a leftover holding the pipe is
        // still abandoned. The reader it leaves blocks until that leftover
        // exits — a thread per stuck check, bounded by the signed-in memo.
        let _ = finished.recv_timeout(within.saturating_sub(started.elapsed()).max(Duration::from_millis(50)));
        std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()))
    };
    Ok(Some(Output { status, stdout: take(out), stderr: take(err) }))
}

/// Whether the child has exited, without reaping it: its pid stays its own
/// (a zombie) until `wait`, so `kill_group` cannot reach a group id the
/// system has since handed to someone else.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: waitid writes only into the zeroed siginfo it is handed;
    // WNOWAIT leaves the child to be reaped by `wait`.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

/// Elsewhere `try_wait` reaps the child as it reports it, so the group is
/// signalled after its leader's pid is free. Accepted: the pid would have
/// to be reused, as a group id, within the few microseconds in between.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    Ok(child.try_wait()?.is_some())
}
