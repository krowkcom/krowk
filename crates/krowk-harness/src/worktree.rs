//! Worktrees (WT3): a git worktree of its own for an agent, so agents
//! running at once edit their own files on their own branch rather than
//! overwrite each other's in one checkout and fight over one index.
//!
//! No model is involved here: `create` makes one, `prepare` readies it,
//! `finish` removes it when the agent left it as it found it and keeps it
//! otherwise. The subagent tool calls them around a child's turn
//! (`isolation: "worktree"`), and a top-level session can call them around
//! its own.
//!
//! - **Where**: `<root>/<repo-id>/<hex>`, the root being
//!   `krowk_api::home::worktrees_root` (outside krowk's home, which the
//!   sandbox hides), the repo-id the first 16 hex digits of the sha256 of
//!   the canonical common git directory, `<hex>` 8 random hex digits. The
//!   branch is `krowk/<hex>`, which is what the sandbox lets a commit move
//!   (`crate::sandbox::managed_worktree`).
//! - **Base** (WT14): the working state of the directory it is made from,
//!   so a child started mid-edit sees the parent's edits. A clean checkout
//!   gives its HEAD. Otherwise a commit on top of HEAD of what `git add -A`
//!   would stage — modified, new and deleted files, `.gitignore` respected —
//!   built in a fresh index file of krowk's, so the parent's index, HEAD and
//!   files are never touched. `add -A` runs the repository's clean filters
//!   (git-lfs's, say), as `git status` does: their commands come from
//!   config, which is the person's own. Every later judgement of the
//!   worktree (unchanged, commits ahead) is against that commit.
//! - **One at a time per repository**: `git worktree add` and the like
//!   write files in the common directory (`config`, `worktrees/`), and two
//!   at once fail on its `config.lock`. So every change here holds `lock`:
//!   a mutex for this process's threads, and `File::lock` on
//!   `<common>/krowk-worktree.lock` for every other krowk.
//! - **Locked while it runs**: `git worktree lock --reason krowk:<owner>`,
//!   the owner being the session that works in it, so `git worktree prune`
//!   leaves it alone and a listing can say whose it is.
//! - **Every git through `krowk_api::git`**: hooks off, so `worktree add`
//!   runs no `post-checkout` of the repository's, outside any sandbox.
//! - **Prepared** by `STEPS` before its agent starts: submodules first
//!   (`submodules`), then the main checkout's build output cloned in where
//!   that is nearly free (`seed`), then the ignored files its
//!   `.worktreeinclude` lists, copied (`include`), and last the
//!   repository's setup command, in the sandbox (`setup`). A step that
//!   falls short leaves a note, and the notes open the agent's first
//!   prompt (`first_prompt`).
//! - **Recorded, and held while in use** (WT9, `manage`): its base, its
//!   session and when it was made in `<root>/<repo-id>/<hex>.json`, the
//!   repository's common directory in `<root>/<repo-id>/common`, and
//!   `<hex>.live` locked by the process whose session works in it
//!   (`create_held`), so `krowk worktrees` can list, remove and prune
//!   kept ones safely.
//! - **A port slot** (`setup::port_slot`) is held by whoever uses the
//!   worktree, for as long as they do: `KROWK_PORT_BASE` for the setup
//!   command and the agent's commands.

pub mod include;
pub mod manage;
pub mod seed;
pub mod setup;

use crate::instances::WorktreesConfig;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};

/// What a krowk worktree's branch is named under.
pub const BRANCH_PREFIX: &str = "krowk/";
/// The file in the common directory every krowk locks to change worktrees.
pub const LOCK_FILE: &str = "krowk-worktree.lock";
/// What a krowk worktree's lock reason starts with; the owner follows.
pub const LOCK_REASON: &str = "krowk:";

/// One worktree krowk made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Its directory, `<root>/<repo-id>/<hex>`: the agent's cwd.
    pub path: PathBuf,
    /// The 8 hex digits its directory and branch are named by.
    pub hex: String,
    /// The commit it was made at: the parent's HEAD, or a commit on top of
    /// it holding the parent's uncommitted changes (`working_state`).
    pub base: String,
    /// The repository's common git directory, canonical.
    pub common: PathBuf,
    /// The repository's main checkout (a bare repository's own directory).
    pub main: PathBuf,
    /// `repo_id(common)`: the directory under the root it is in.
    pub repo_id: String,
}

impl Worktree {
    /// `krowk/<hex>`.
    pub fn branch(&self) -> String {
        format!("{BRANCH_PREFIX}{}", self.hex)
    }

    /// `<root>/<repo-id>`: the directory of krowk's own for its repository,
    /// which no sandbox can write, its worktrees inside.
    pub fn repo_dir(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }
}

/// Why a worktree could not be made, or finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The directory is in no git repository.
    NotARepository,
    /// git, or the file system, said no: what it said.
    Failed(String),
    /// The turn it was for was interrupted while it was readied: it is gone.
    Interrupted,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotARepository => f.write_str("not in a git repository"),
            Error::Failed(why) => f.write_str(why),
            Error::Interrupted => f.write_str("interrupted"),
        }
    }
}

impl std::error::Error for Error {}

/// How `finish` left a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// Unchanged and clean: the directory, the branch and git's record of
    /// it are gone.
    Removed,
    /// Changed: unlocked, and kept with its branch, for the person.
    Kept { commits: usize, dirty: bool },
}

impl Finished {
    /// The line the agent that started it is told when it is kept.
    pub fn note(&self, wt: &Worktree) -> Option<String> {
        match self {
            Finished::Removed => None,
            Finished::Kept { commits, dirty } => {
                Some(format!("Worktree: {} (branch {}, {commits} commits, uncommitted changes: {})", wt.path.display(), wt.branch(), if *dirty { "yes" } else { "no" }))
            }
        }
    }
}

/// What a prepare step is handed. The inputs a step needs join it as
/// fields, so a step's signature stays one argument.
pub struct Prepare<'a> {
    pub worktree: &'a Worktree,
    /// The person's `worktrees` config.
    pub config: &'a WorktreesConfig,
    /// The repository's own `worktrees` config, from its
    /// `.krowk/config.json` (`permissions::settings::Loaded::worktrees`).
    pub project: Option<&'a WorktreesConfig>,
    /// Whether the person trusts the repository the worktree is of: the
    /// parent session's decision, as for its hooks.
    pub trusted: bool,
    /// What the setup command runs in (`setup::sandbox`): none where this
    /// machine has no sandbox.
    pub sandbox: Option<&'a crate::sandbox::Plan>,
    /// The worktree's `KROWK_PORT_BASE`, from the port slot its user holds
    /// (`setup::port_slot`); none without one.
    pub port_base: Option<u16>,
    /// The build slots a heavy setup command waits for.
    pub builds: Option<&'a crate::builds::Builds>,
    /// Set when the turn the worktree is for is interrupted: the setup
    /// command, or its wait for a build slot, stops, and no later step
    /// starts (`prepare_or_discard`).
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
}

impl<'a> Prepare<'a> {
    /// The steps' inputs for `worktree` with `config` alone: an untrusted
    /// repository, no sandbox, no port slot, no build slots.
    pub fn new(worktree: &'a Worktree, config: &'a WorktreesConfig) -> Prepare<'a> {
        Prepare { worktree, config, project: None, trusted: false, sandbox: None, port_base: None, builds: None, cancel: None }
    }
}

/// One step that readies a new worktree before its agent starts. It does
/// not fail the creation: a step that cannot do its part returns a note
/// saying why, which the agent reads at the top of its first prompt
/// (`first_prompt`), and the agent starts anyway. `None` when there is
/// nothing to say.
pub type Step = fn(&Prepare<'_>) -> Option<String>;

/// The prepare steps, run in this order after `create` and before the
/// agent's first turn: submodules, seed, include, setup.
pub const STEPS: &[(&str, Step)] = &[("submodules", submodules), ("seed", seed::seed), ("include", include::include), ("setup", setup::setup)];

/// Runs `STEPS`, in order: their notes, in the same order. Blocking: off
/// the async runtime.
pub fn prepare(p: &Prepare<'_>) -> Vec<String> {
    STEPS.iter().filter_map(|(_, step)| step(p)).collect()
}

/// `prepare`, unless the turn is interrupted (`Prepare::cancel`) before
/// or while it runs: then the worktree, never worked in, is removed with
/// its branch whatever the steps left in it, and `Error::Interrupted`.
pub fn prepare_or_discard(p: &Prepare<'_>) -> Result<Vec<String>, Error> {
    let cancelled = || p.cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));
    let notes = if cancelled() { Vec::new() } else { prepare(p) };
    if !cancelled() {
        return Ok(notes);
    }
    discard(p.worktree)?;
    Err(Error::Interrupted)
}

/// A worktree no agent worked in, removed with its branch, under the lock.
pub fn discard(wt: &Worktree) -> Result<(), Error> {
    let _held = lock(&wt.common)?;
    let _ = read(git(&wt.main)?.args(["worktree", "unlock"]).arg(&wt.path), "worktree unlock");
    remove(wt, true)
}

/// The first prompt of the agent that works in a prepared worktree: the
/// prepare steps' notes, each marked as krowk's so the model does not take
/// it for its parent's words, then `prompt`.
pub fn first_prompt(notes: &[String], prompt: &str) -> String {
    let mut out = String::new();
    for note in notes {
        out.push_str("Note from krowk, which prepared this worktree: ");
        out.push_str(note.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(prompt);
    out
}

/// How deep `submodules` follows submodules inside submodules: a bound on
/// what a repository's `.gitmodules` can make krowk clone.
const SUBMODULE_DEPTH: usize = 8;

/// The first prepare step (WT15). `git worktree add` leaves a submodule an
/// empty directory, so a build fails in every new worktree of a repository
/// that has any. Each one `.gitmodules` lists, submodules inside
/// submodules too, is initialised at the commit the worktree records:
///
/// - **From the main checkout, without the network**, when the main
///   checkout has it initialised: cloned from that submodule's git
///   directory (`submodule.<name>.url` pointed there for the one call), so
///   the objects are hard-linked or copied locally and nothing is
///   downloaded, then its `origin` set back to the submodule's own URL, so
///   a later fetch there goes where the main checkout's does. A local
///   clone has no alternates, so a `gc` on either side cannot take objects
///   the other needs. If that clone lacks the commit, it is fetched from
///   the submodule's own URL.
/// - **From its URL** otherwise, as `git submodule update --init` would,
///   in a session of its own with no terminal (ssh's host-key and
///   passphrase prompts open `/dev/tty`, which `GIT_TERMINAL_PROMPT` does
///   not cover) and stopped after `SUBMODULE_TIMEOUT`.
///
/// The repository's config is shared with the main checkout, so a
/// submodule of the worktree's own is not `git submodule init`ed there:
/// that would write `submodule.<name>.url` and `.active` for the main
/// checkout too, bring back one the person deinitialised, outlive the
/// worktree, and race a creation beside it on `config.lock`. Its URL (the
/// config's when the person set one, else `.gitmodules`'s, a relative one
/// resolved against the repository's `origin`) and `active` are given to
/// the one `submodule update` instead. A submodule's own submodules are
/// `init`ed: their config is the submodule's, in the worktree's admin
/// directory.
///
/// Its git directory lands in the worktree's admin directory
/// (`<common>/worktrees/<name>/modules/`), which the sandbox binds
/// read-only (WT4: a submodule's config is there), so it goes with the
/// worktree and the agent can edit a submodule's files but not commit in
/// it. Nothing the repository's files name is run: hooks are off
/// (`krowk_api::git`), `ext::` URLs are refused whatever config says, a
/// local path from `.gitmodules` is refused as git refuses it by default
/// (only the main checkout's own git directory, which krowk chose, is
/// cloned locally), `--checkout` overrides any `submodule.<name>.update`,
/// and git itself refuses a `!command` one from `.gitmodules`. Filters
/// from config still run, as on any checkout. A submodule that cannot be
/// initialised stays empty, and the note names it with what git said.
fn submodules(p: &Prepare<'_>) -> Option<String> {
    let mut failed = Vec::new();
    init_submodules(&p.worktree.path, &p.worktree.main, "", 0, &mut failed);
    (!failed.is_empty()).then(|| failure_note(&failed))
}

/// The note for the submodules in `failed`: at most `MAX_NOTED` of them,
/// each entry cut to `MAX_NOTE_LINE`.
fn failure_note(failed: &[String]) -> String {
    let more = failed.len().saturating_sub(MAX_NOTED);
    let mut lines: Vec<String> = failed.iter().take(MAX_NOTED).map(|l| bounded(l, MAX_NOTE_LINE)).collect();
    if more > 0 {
        lines.push(format!("- and {more} more"));
    }
    format!("these submodules could not be initialised and are empty directories here:\n{}", lines.join("\n"))
}

/// How many failed submodules a note names, and how long each one's
/// entry may be: a repository with hundreds must not fill the prompt.
const MAX_NOTED: usize = 20;
const MAX_NOTE_LINE: usize = 300;

/// How long one submodule may take to come from its URL.
const SUBMODULE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// `text` cut to `max` bytes at a character boundary, marked when cut.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Initialises the submodules of the checkout `dir`, whose counterpart in
/// the main checkout is `main`, then theirs; a line in `failed` for each
/// that could not be, `prefix` being `dir`'s path from the worktree's top.
fn init_submodules(dir: &Path, main: &Path, prefix: &str, depth: usize, failed: &mut Vec<String>) {
    // No `.gitmodules`, no git run.
    if !dir.join(".gitmodules").symlink_metadata().is_ok_and(|m| m.is_file()) {
        return;
    }
    if depth >= SUBMODULE_DEPTH {
        failed.push(format!("- {prefix}: submodules nested more than {SUBMODULE_DEPTH} deep are not initialised"));
        return;
    }
    let listed = match listed_submodules(dir) {
        Ok(l) => l,
        Err(e) => return failed.push(format!("- {prefix}.gitmodules: {}", last_lines(&e.to_string()))),
    };
    for (name, path) in listed {
        let shown = format!("{prefix}{path}");
        match init_submodule(dir, &main.join(&path), &name, &path, depth == 0) {
            Ok(()) => init_submodules(&dir.join(&path), &main.join(&path), &format!("{shown}/"), depth + 1, failed),
            Err(e) => failed.push(format!("- {shown}: {}", last_lines(&e.to_string()))),
        }
    }
}

/// The submodules `dir`'s `.gitmodules` lists whose path is a submodule in
/// its index, as (name, path), by path. Read as a file with no includes:
/// it is the repository's content.
fn listed_submodules(dir: &Path) -> Result<Vec<(String, String)>, Error> {
    let links = gitlinks(&read(query(dir)?.args(["ls-files", "--stage", "-z"]), "ls-files")?);
    let out = query(dir)?
        .args(["config", "--no-includes", "-z", "--file"])
        .arg(dir.join(".gitmodules"))
        .args(["--get-regexp", r"^submodule\..*\.path$"])
        .output()
        .map_err(|e| Error::Failed(format!("git config: {e}")))?;
    // 1: no submodule has a path.
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(Error::Failed(format!("git config: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let mut listed: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.split_once('\n'))
        .filter_map(|(key, path)| Some((key.strip_prefix("submodule.")?.strip_suffix(".path")?.to_string(), path.to_string())))
        .filter(|(_, path)| links.contains(path))
        .collect();
    listed.sort_by(|a, b| a.1.cmp(&b.1));
    listed.dedup_by(|a, b| a.1 == b.1);
    Ok(listed)
}

/// The submodule `name` at `path` in `dir`, initialised and checked out:
/// from `main`, its counterpart in the main checkout, when that is
/// initialised, else from its URL (see `submodules`). `top` for one of
/// the worktree's own, whose config is shared with the main checkout and
/// so is not written.
fn init_submodule(dir: &Path, main: &Path, name: &str, path: &str, top: bool) -> Result<(), Error> {
    let url_key = format!("submodule.{name}.url");
    let url = if top {
        top_url(dir, name)?
    } else {
        read(git(dir)?.args(["submodule", "init", "--quiet", "--", path]), "submodule init")?;
        read(query(dir)?.args(["config", "--get", &url_key]), "config")?
    };
    let active = (format!("submodule.{name}.active"), "true");
    if let Some(local) = own_git_dir(main) {
        let cloned = update(dir, path, &[(&url_key, local.as_os_str()), (&active.0, active.1.as_ref()), ("protocol.file.allow", "always".as_ref())], false);
        // Only a repository of its own: an empty directory would be the
        // worktree's, and its config the person's.
        if let Some(own) = own_git_dir(&dir.join(path)) {
            read(git(dir)?.arg("config").arg("--file").arg(own.join("config")).args(["remote.origin.url", &url]), "config")?;
        }
        if cloned.is_ok() {
            return Ok(());
        }
    }
    update(dir, path, &[(&url_key, url.as_ref()), (&active.0, active.1.as_ref()), ("protocol.file.allow", "user".as_ref())], true)
}

/// The URL of the worktree's own submodule `name`, as `submodule init`
/// would record it: the config's when set, else `.gitmodules`'s, resolved
/// against `origin` when relative.
fn top_url(dir: &Path, name: &str) -> Result<String, Error> {
    let key = format!("submodule.{name}.url");
    if let Ok(url) = read(query(dir)?.args(["config", "--get", &key]), "config") {
        return Ok(url);
    }
    let listed = read(query(dir)?.args(["config", "--no-includes", "--file"]).arg(dir.join(".gitmodules")).args(["--get", &key]), "config")?;
    if !listed.starts_with("./") && !listed.starts_with("../") {
        return Ok(listed);
    }
    // No origin: relative to the superproject's own directory, as git
    // does.
    let base = read(query(dir)?.args(["config", "--get", "remote.origin.url"]), "config").unwrap_or_else(|_| dir.to_string_lossy().into_owned());
    Ok(resolve_url(&base, &listed))
}

/// `url`, starting `./` or `../`, resolved against `base` as git resolves a
/// submodule's: each `../` drops one component of `base`, the host part of
/// an scp-like `host:path` kept.
fn resolve_url(base: &str, url: &str) -> String {
    let mut base = base.trim_end_matches('/').to_string();
    let mut rest = url;
    let mut sep = "/";
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            match base.rfind(['/', ':']) {
                Some(i) => {
                    sep = if base[i..].starts_with(':') { ":" } else { "/" };
                    base.truncate(i);
                }
                None => base.clear(),
            }
        } else {
            break;
        }
    }
    format!("{base}{sep}{rest}")
}

/// `git submodule update --checkout` of `path` in `dir`, `ext::` refused,
/// with `config` on top, passed as `GIT_CONFIG_KEY_<n>` so a submodule's
/// name needs no quoting. No credential prompt: nobody is at the terminal
/// for it, and it would draw over krowk's. `remote` for one that may reach
/// the network: with no terminal, and stopped after `SUBMODULE_TIMEOUT`.
fn update(dir: &Path, path: &str, config: &[(&str, &std::ffi::OsStr)], remote: bool) -> Result<(), Error> {
    let mut c = git(dir)?;
    let config: Vec<(&str, &std::ffi::OsStr)> = std::iter::once(("protocol.ext.allow", "never".as_ref())).chain(config.iter().copied()).collect();
    for (i, (key, value)) in config.iter().enumerate() {
        c.env(format!("GIT_CONFIG_KEY_{i}"), key).env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    c.env("GIT_CONFIG_COUNT", config.len().to_string()).env("GIT_TERMINAL_PROMPT", "0");
    c.args(["submodule", "update", "--quiet", "--checkout", "--", path]);
    if !remote {
        return read(&mut c, "submodule update").map(drop);
    }
    let within = crate::readiness::Probe { dir: dir.to_path_buf(), within: SUBMODULE_TIMEOUT };
    match crate::readiness::output_detached(&mut c, &within) {
        Ok(Some(out)) if out.status.success() => Ok(()),
        Ok(Some(out)) => Err(Error::Failed(format!("git submodule update: {}", String::from_utf8_lossy(&out.stderr).trim()))),
        Ok(None) => Err(Error::Failed(format!("git submodule update: stopped after {} s", SUBMODULE_TIMEOUT.as_secs()))),
        Err(e) => Err(Error::Failed(format!("git submodule update: {e}"))),
    }
}

/// The git directory of the repository whose top is `dir`: none when `dir`
/// is missing, or is not a repository's top (an uninitialised submodule's
/// empty directory is its superproject's).
fn own_git_dir(dir: &Path) -> Option<PathBuf> {
    let out = read(query(dir).ok()?.args(["rev-parse", "--show-toplevel", "--absolute-git-dir"]), "rev-parse").ok()?;
    let (top, git_dir) = out.split_once('\n')?;
    let same = Path::new(top).canonicalize().ok()? == dir.canonicalize().ok()?;
    same.then(|| PathBuf::from(git_dir))
}

/// The last lines of what git said: enough to say why, not a page of it.
fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    lines[lines.len().saturating_sub(10)..].join("\n  ")
}

/// A new worktree of the repository `cwd` is in, at its working state
/// (`working_state`), under `root`, locked for `owner` (the session that
/// will work in it). Blocking: off the async runtime. Nobody holds it live
/// (`manage::Held`): what a session works in comes from `create_held`.
pub fn create(cwd: &Path, root: &Path, owner: &str) -> Result<Worktree, Error> {
    create_held(cwd, root, owner).map(|(wt, _)| wt)
}

/// `create`, held live for `owner` (`manage::hold`) from before the
/// repository's lock is let go, so no `krowk worktrees remove` finds it
/// unheld in between; recorded (`manage::Record`) for listing it later.
pub fn create_held(cwd: &Path, root: &Path, owner: &str) -> Result<(Worktree, manage::Held), Error> {
    let common = match read(query(cwd)?.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]), "rev-parse") {
        Ok(c) if !c.is_empty() => PathBuf::from(c).canonicalize().map_err(|e| Error::Failed(format!("the repository's git directory: {e}")))?,
        _ => return Err(Error::NotARepository),
    };
    let head = read(query(cwd)?.args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]), "rev-parse HEAD")
        .ok()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Error::Failed("the repository has no commit yet to start a worktree from".into()))?;
    let main = main_checkout(cwd, &common)?;
    let repo_id = repo_id(&common);
    let dir = root.join(&repo_id);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Failed(format!("create {}: {e}", dir.display())))?;
    manage::note_common(&dir, &common);
    // Hashing the files is the slow part: done before the lock, so
    // creations at once in one repository do not queue behind it.
    let tree = working_state(cwd, &head, &dir)?;
    let _held = lock(&common)?;
    let (hex, path) = std::iter::repeat_with(random_hex).map(|h| (h.clone(), dir.join(h))).take(16).find(|(_, p)| !p.exists()).ok_or_else(|| Error::Failed(format!("no free name for a worktree in {}", dir.display())))?;
    let base = match tree {
        Some(tree) => commit_state(cwd, &tree, &head, &hex)?,
        None => head,
    };
    let wt = Worktree { path, hex, base, common, main, repo_id };
    read(git(cwd)?.args(["worktree", "add", "--quiet", "--no-track", "-b"]).arg(wt.branch()).arg(&wt.path).arg(&wt.base), "worktree add")?;
    let locked = read(git(&wt.main)?.args(["worktree", "lock", "--reason"]).arg(format!("{LOCK_REASON}{owner}")).arg(&wt.path), "worktree lock");
    match locked.and_then(|_| manage::write_record(&wt, owner)).and_then(|()| manage::hold(&wt)) {
        Ok(held) => Ok((wt, held)),
        Err(e) => {
            let _ = remove(&wt, true);
            Err(e)
        }
    }
}

/// The tree of `cwd`'s working state when it differs from `head`'s: none
/// for a clean checkout, or a bare repository's directory. `git status`
/// judges clean, held to every untracked file whatever
/// `status.showUntrackedFiles` says; changes it shows that a tree cannot
/// hold (a submodule's own edits) give `head`'s tree, so none as well.
///
/// The tree is built in a scratch index in `scratch`, never the
/// repository's own: a copy of it, so what the person staged beyond what
/// `add --all` finds (a file force-added past `.gitignore`, an
/// intent-to-add, a skip-worktree entry) is kept, and its stat cache spares
/// hashing every file again. git replaces an index by rename, so the copy
/// is one whole index; it keeps the original's mtime, which git's
/// racy-clean check judges entries against. A repository with no index
/// yet starts from `head`'s tree. A new repository nested in the checkout
/// (`vendor/foo/` with its own `.git`) is not carried: `add --all` would
/// record it as a commit the worktree cannot check out, an empty directory.
fn working_state(cwd: &Path, head: &str, scratch: &Path) -> Result<Option<String>, Error> {
    let top = match read(query(cwd)?.args(["rev-parse", "--show-toplevel"]), "rev-parse") {
        Ok(t) if !t.is_empty() => PathBuf::from(t),
        _ => return Ok(None),
    };
    let status = read(query(&top)?.args(["status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none"]), "status")?;
    if status.is_empty() {
        return Ok(None);
    }
    let index = Scratch(std::path::absolute(scratch.join(format!(".index-{}", random_hex()))).map_err(|e| Error::Failed(format!("the index for the working state: {e}")))?);
    // Split or not, the scratch index is written whole: no shared index
    // file of krowk's is left in the repository's git directory.
    let staged = || git(&top).map(|mut c| {
        c.env("GIT_INDEX_FILE", &index.0).args(["-c", "core.splitIndex=false"]);
        c
    });
    let own = PathBuf::from(read(query(&top)?.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]), "rev-parse")?);
    match copy_index(&own, &index.0) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => drop(read(staged()?.args(["read-tree", head]), "read-tree")?),
        Err(e) => return Err(Error::Failed(format!("copy {}: {e}", own.display()))),
    }
    let mut known = gitlinks(&read(staged()?.args(["ls-files", "--stage", "-z"]), "ls-files")?);
    known.extend(gitlinks(&read(query(&top)?.args(["ls-tree", "-r", "-z", "--full-tree", head]), "ls-tree")?));
    read(staged()?.args(["add", "--all", "--", ":/"]), "add")?;
    let nested: Vec<String> = gitlinks(&read(staged()?.args(["ls-files", "--stage", "-z"]), "ls-files")?).into_iter().filter(|p| !known.contains(p)).collect();
    if !nested.is_empty() {
        read(staged()?.args(["update-index", "--force-remove", "--"]).args(&nested), "update-index")?;
    }
    let tree = read(staged()?.arg("write-tree"), "write-tree")?;
    let head_tree = read(query(&top)?.args(["rev-parse", &format!("{head}^{{tree}}")]), "rev-parse")?;
    Ok((tree != head_tree).then_some(tree))
}

/// `from`, the repository's index, copied to `to` with its mtime.
fn copy_index(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let modified = src.metadata()?.modified()?;
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.set_modified(modified)
}

/// The paths of the gitlinks (mode 160000) in `ls-files --stage -z` or
/// `ls-tree -r -z` output: both start an entry with its mode.
fn gitlinks(listing: &str) -> std::collections::HashSet<String> {
    listing.split('\0').filter_map(|e| e.split_once('\t')).filter(|(meta, _)| meta.starts_with("160000 ")).map(|(_, p)| p.to_string()).collect()
}

/// The commit of the working state `tree` on top of `head`, for the
/// worktree `hex`. The person's identity when git has one, krowk's when it
/// has none: it is krowk's commit, and a person without `user.email` set
/// must still get a worktree.
fn commit_state(cwd: &Path, tree: &str, head: &str, hex: &str) -> Result<String, Error> {
    let message = format!("krowk: working state for {hex}");
    let commit = |identity: &[&str]| read(git(cwd)?.args(identity).args(["commit-tree", tree, "-p", head, "-m", &message]), "commit-tree");
    commit(&[]).or_else(|_| commit(&["-c", "user.name=krowk", "-c", "user.email=krowk@localhost"]))
}

/// A scratch index file, gone with its lock file when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// When the agent is done: unchanged (HEAD still the base, `git status`
/// empty, so every submodule at its recorded commit and clean) it is
/// removed with its branch, the submodules' git directories with its admin
/// directory; otherwise unlocked and kept.
/// Unlocked on every way out: the session is over. Blocking: off the async
/// runtime.
pub fn finish(wt: &Worktree) -> Result<Finished, Error> {
    let _held = lock(&wt.common)?;
    let judged = changes(wt);
    // One that was never locked is not a reason to stop.
    let _ = read(git(&wt.main)?.args(["worktree", "unlock"]).arg(&wt.path), "worktree unlock");
    let (head, status) = judged?;
    if head == wt.base && status.is_empty() {
        remove(wt, false)?;
        return Ok(Finished::Removed);
    }
    let range = format!("{}..{head}", wt.base);
    let commits = read(query(&wt.path)?.args(["rev-list", "--count", &range]), "rev-list")?.parse().unwrap_or(0);
    Ok(Finished::Kept { commits, dirty: !status.is_empty() })
}

/// The worktree's HEAD, and its `git status --porcelain`: every untracked
/// file and submodule change counted whatever the repository's config
/// says (`status.showUntrackedFiles=no` would hide a new file, and the
/// removal would take it). With submodules, each checked-out one's own
/// status too, at any depth (`submodule_changes`).
fn changes(wt: &Worktree) -> Result<(String, String), Error> {
    let head = read(query(&wt.path)?.args(["rev-parse", "HEAD"]), "rev-parse HEAD")?;
    let mut status = read(query(&wt.path)?.args(["status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none"]), "status")?;
    if has_submodules(&wt.path) {
        status.push_str(&submodule_changes(&wt.path, "", 0)?);
    }
    Ok((head, status))
}

/// Whether the worktree at `path` may have submodules checked out: what
/// `remove` needs a `--force` for, and `changes` looks into.
fn has_submodules(path: &Path) -> bool {
    path.join(".gitmodules").symlink_metadata().is_ok()
}

/// The status of every submodule checked out in `dir`, and of theirs, each
/// judged in itself with the same flags as the worktree: git's own status
/// passes neither flag down, so a submodule's `status.showUntrackedFiles=no`
/// would hide a new file in it, and its `.gitmodules`'
/// `submodule.<name>.ignore=all` an edit in a submodule of its own, and the
/// forced removal would take them. Each line is prefixed with the
/// submodule's path. A path that is a symlink is not followed, it shows in
/// its superproject's status; nesting past `SUBMODULE_DEPTH` is an error,
/// so the worktree is kept.
fn submodule_changes(dir: &Path, prefix: &str, depth: usize) -> Result<String, Error> {
    if depth > SUBMODULE_DEPTH {
        return Err(Error::Failed(format!("{prefix}: submodules nested more than {SUBMODULE_DEPTH} deep")));
    }
    let mut links: Vec<String> = gitlinks(&read(query(dir)?.args(["ls-files", "--stage", "-z"]), "ls-files")?).into_iter().collect();
    links.sort();
    let mut out = String::new();
    for path in links {
        let sub = dir.join(&path);
        if sub.symlink_metadata().map_or(true, |m| !m.is_dir()) || own_git_dir(&sub).is_none() {
            continue;
        }
        let shown = format!("{prefix}{path}/");
        let status = read(query(&sub)?.args(["status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none"]), "status")?;
        for line in status.lines() {
            out.push_str(&format!("{shown}: {line}\n"));
        }
        out.push_str(&submodule_changes(&sub, &shown, depth + 1)?);
    }
    Ok(out)
}

/// The worktree and its branch gone, under the caller's `lock`. `force`
/// for one being rolled back, which may hold the checkout's files only.
fn remove(wt: &Worktree, force: bool) -> Result<(), Error> {
    let mut c = git(&wt.main)?;
    // `worktree remove` judges "clean" by the same status config: held
    // to every untracked file, as `changes` is.
    c.args(["-c", "status.showUntrackedFiles=normal", "worktree", "remove"]);
    if force {
        c.args(["--force", "--force"]);
    } else if has_submodules(&wt.path) {
        // git refuses to remove a worktree with submodules checked out
        // without one `--force`, which also skips its own clean check:
        // `finish` has judged it unchanged, submodules included, under
        // the lock.
        c.arg("--force");
    }
    read(c.arg(&wt.path), "worktree remove")?;
    manage::forget(wt.repo_dir(), &wt.hex);
    read(git(&wt.main)?.args(["branch", "--quiet", "-D"]).arg(wt.branch()), "branch -D").map(drop)
}

/// The first 16 hex digits of the sha256 of `common`, the canonical common
/// git directory: one directory under the root per repository.
pub fn repo_id(common: &Path) -> String {
    let digest = Sha256::digest(common.as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 8 random hex digits.
fn random_hex() -> String {
    let mut b = [0u8; 4];
    // The system's RNG does not fail where krowk runs; the clock is a
    // fallback that only makes a name less unlikely to be taken.
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).is_err() {
        b = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The repository's main checkout: the first worktree git lists.
fn main_checkout(cwd: &Path, common: &Path) -> Result<PathBuf, Error> {
    let list = read(query(cwd)?.args(["worktree", "list", "--porcelain"]), "worktree list")?;
    let first = list.lines().find_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from);
    Ok(first.unwrap_or_else(|| common.to_path_buf()))
}

/// Repositories whose worktrees a thread of this process is changing.
static BUSY: (Mutex<Vec<PathBuf>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());

/// Held while a repository's worktrees change: see `lock`. The file goes
/// first, so the OS lock is free by the time the next thread is let in.
pub struct RepoLock {
    _file: std::fs::File,
    _key: Busy,
}

/// This process's claim on one repository, given up on drop.
struct Busy(PathBuf);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut busy = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        busy.retain(|c| c != &self.0);
        BUSY.1.notify_all();
    }
}

/// The per-repository lock every change to krowk's worktrees of the
/// repository whose common directory is `common` holds, waited for: this
/// process's threads queue on a mutex, and every krowk process on
/// `File::lock` of `<common>/krowk-worktree.lock`, which the OS frees when a
/// holder dies. Both, so a thread never depends on how an OS scopes a file
/// lock within one process.
pub fn lock(common: &Path) -> Result<RepoLock, Error> {
    let key = {
        let mut busy = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        while busy.iter().any(|c| c == common) {
            busy = BUSY.1.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.push(common.to_path_buf());
        Busy(common.to_path_buf())
    };
    let at = common.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&at).map_err(|e| Error::Failed(format!("open {}: {e}", at.display())))?;
    file.lock().map_err(|e| Error::Failed(format!("lock {}: {e}", at.display())))?;
    Ok(RepoLock { _file: file, _key: key })
}

/// git that changes something in `dir`, through `krowk_api::git`, with no
/// `GIT_DIR` and the like an outer git (a hook running krowk) may have
/// set: every call here names its repository by its directory.
fn git(dir: &Path) -> Result<Command, Error> {
    let mut c = krowk_api::git::command(dir).map_err(|e| Error::Failed(format!("git: {e}")))?;
    clean(&mut c);
    Ok(c)
}

/// `git` for a call that only reads.
fn query(dir: &Path) -> Result<Command, Error> {
    let mut c = krowk_api::git::query(dir).map_err(|e| Error::Failed(format!("git: {e}")))?;
    clean(&mut c);
    Ok(c)
}

fn clean(c: &mut Command) {
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_PREFIX"] {
        c.env_remove(k);
    }
}

/// What git printed, trimmed, or what it said when it failed.
fn read(c: &mut Command, what: &str) -> Result<String, Error> {
    let out = c.output().map_err(|e| Error::Failed(format!("git {what}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Failed(format!("git {what}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit, and a worktrees root beside it.
    pub(super) fn repo(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        repo_in(&std::env::temp_dir(), name)
    }

    /// `repo`, in `dir`.
    pub(super) fn repo_in(dir: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = dir.join(format!("krowk-worktree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git_ok(&main, &["init", "-q", "-b", "main"]);
        std::fs::write(main.join("a.txt"), "a\n").unwrap();
        git_ok(&main, &["add", "a.txt"]);
        git_ok(&main, &["commit", "-q", "-m", "one"]);
        (base.clone(), main, base.join("worktrees"))
    }

    /// git in a test repository with none of this machine's config.
    pub(super) fn git_ok(dir: &Path, args: &[&str]) -> String {
        let o = krowk_api::git::command(dir).unwrap().args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    pub(super) fn has_git() -> bool {
        krowk_api::git::query(Path::new(".")).is_ok_and(|mut c| c.arg("--version").output().is_ok_and(|o| o.status.success()))
    }

    /// Eight at once, as a response of eight subagents starts them: each its
    /// own directory and branch, none failing on another's `config.lock`,
    /// each one the sandbox takes for krowk's, and each removed again.
    #[test]
    fn wt3_eight_worktrees_made_at_once_all_succeed() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("eight");
        let made: Vec<Result<Worktree, Error>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|i| {
                let (main, root) = (&main, &root);
                s.spawn(move || create(main, root, &format!("owner-{i}")))
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let made: Vec<Worktree> = made.into_iter().map(|r| r.expect("every creation succeeds")).collect();
        let mut dirs: Vec<&Path> = made.iter().map(|w| w.path.as_path()).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), 8, "eight directories");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let common = main.join(".git").canonicalize().unwrap();
        let list = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert_eq!(list.matches(&format!("locked {LOCK_REASON}owner-")).count(), 8, "each locked for its owner: {list}");
        for w in &made {
            assert_eq!((w.base.as_str(), &w.common, w.main.canonicalize().unwrap(), w.repo_id.len()), (head.as_str(), &common, main.canonicalize().unwrap(), 16));
            assert_eq!(w.path, root.join(&w.repo_id).join(&w.hex));
            assert!(w.path.join("a.txt").is_file());
            assert_eq!(git_ok(&w.path, &["rev-parse", "--abbrev-ref", "HEAD"]), w.branch());
            let managed = crate::sandbox::managed_worktree(&w.path, &root).expect("the sandbox's krowk worktree");
            assert_eq!(managed.hex, w.hex);
        }
        for w in &made {
            assert_eq!(finish(w).unwrap(), Finished::Removed);
        }
        assert_eq!(git_ok(&main, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Unchanged leaves nothing; an edit, or a commit, keeps the worktree
    /// and its branch, and the note names both.
    #[test]
    fn wt3_an_unchanged_worktree_is_removed_and_a_changed_one_kept() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("finish");
        let w = create(&main, &root, "s1").unwrap();
        // An ignored build output is not a change.
        std::fs::write(main.join(".git/info/exclude"), "target/\n").unwrap();
        std::fs::create_dir_all(w.path.join("target")).unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert!(!w.path.exists());
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");
        assert!(!git_ok(&main, &["worktree", "list"]).contains(&w.hex));

        let w = create(&main, &root, "s2").unwrap();
        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        let kept = finish(&w).unwrap();
        assert_eq!(kept, Finished::Kept { commits: 0, dirty: true });
        assert_eq!(kept.note(&w).unwrap(), format!("Worktree: {} (branch krowk/{}, 0 commits, uncommitted changes: yes)", w.path.display(), w.hex));
        assert!(w.path.join("a.txt").is_file());
        assert_eq!(git_ok(&main, &["branch", "--list", "--format=%(refname:short)", &w.branch()]), w.branch());
        let list = git_ok(&main, &["worktree", "list", "--porcelain"]);
        assert!(list.contains(&w.hex) && !list.contains("locked"), "unlocked: {list}");

        // A repository whose config hides untracked files: a new file is
        // still a change, and is kept.
        git_ok(&main, &["config", "status.showUntrackedFiles", "no"]);
        let w = create(&main, &root, "s4").unwrap();
        std::fs::write(w.path.join("only-new.txt"), "new\n").unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 0, dirty: true });
        assert!(w.path.join("only-new.txt").is_file());
        git_ok(&main, &["config", "--unset", "status.showUntrackedFiles"]);

        let w = create(&main, &root, "s3").unwrap();
        std::fs::write(w.path.join("b.txt"), "b\n").unwrap();
        git_ok(&w.path, &["add", "b.txt"]);
        git_ok(&w.path, &["commit", "-q", "-m", "two"]);
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 1, dirty: false });
        // A finish that cannot judge it still unlocks it.
        let w = create(&main, &root, "s5").unwrap();
        std::fs::rename(&w.path, w.path.with_extension("moved")).unwrap();
        assert!(finish(&w).is_err());
        std::fs::rename(w.path.with_extension("moved"), &w.path).unwrap();
        assert!(!git_ok(&main, &["worktree", "list", "--porcelain"]).contains(&format!("locked {LOCK_REASON}s5")), "unlocked though its status could not be read");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A parent mid-edit: the worktree starts from its files, a modified, a
    /// new, a deleted and a staged one, from a subdirectory and with
    /// untracked files hidden by config, and is clean there. The parent's
    /// index, HEAD, diffs and untracked files are as they were, and a child
    /// that changes nothing is still removed.
    #[test]
    fn wt14_a_worktree_starts_from_the_parents_uncommitted_changes() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("state");
        std::fs::create_dir_all(main.join("sub")).unwrap();
        for f in ["b.txt", "c.txt", "sub/d.txt"] {
            std::fs::write(main.join(f), format!("{f}\n")).unwrap();
        }
        git_ok(&main, &["add", "."]);
        git_ok(&main, &["commit", "-q", "-m", "two"]);
        git_ok(&main, &["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(main.join(".git/info/exclude"), "*.log\n").unwrap();
        std::fs::write(main.join("a.txt"), "modified\n").unwrap();
        std::fs::write(main.join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(main.join("c.txt")).unwrap();
        std::fs::write(main.join("b.txt"), "staged\n").unwrap();
        git_ok(&main, &["add", "b.txt"]);
        std::fs::write(main.join("build.log"), "ignored\n").unwrap();
        // Force-added past .gitignore, and a repository of its own nested in
        // the checkout, never registered as a submodule.
        std::fs::write(main.join("forced.log"), "forced\n").unwrap();
        git_ok(&main, &["add", "-f", "forced.log"]);
        std::fs::create_dir_all(main.join("vendor/foo")).unwrap();
        git_ok(&main.join("vendor/foo"), &["init", "-q"]);
        std::fs::write(main.join("vendor/foo/x.txt"), "x\n").unwrap();
        git_ok(&main.join("vendor/foo"), &["add", "x.txt"]);
        git_ok(&main.join("vendor/foo"), &["commit", "-q", "-m", "x"]);
        let parent = |main: &Path| ["diff", "diff --cached", "rev-parse HEAD", "ls-files --others --exclude-standard"].map(|a| git_ok(main, &a.split(' ').collect::<Vec<_>>()));
        let before = parent(&main);
        let index = std::fs::read(main.join(".git/index")).unwrap();
        let index_mtime = std::fs::metadata(main.join(".git/index")).unwrap().modified().unwrap();

        let w = create(&main.join("sub"), &root, "s1").unwrap();
        assert_eq!(std::fs::read(main.join(".git/index")).unwrap(), index, "the parent's index is not rewritten");
        assert_eq!(std::fs::metadata(main.join(".git/index")).unwrap().modified().unwrap(), index_mtime);
        assert_eq!(parent(&main), before, "the parent's diffs, HEAD and untracked files are as they were");
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "modified\n");
        assert_eq!(std::fs::read_to_string(w.path.join("b.txt")).unwrap(), "staged\n");
        assert_eq!(std::fs::read_to_string(w.path.join("new.txt")).unwrap(), "new\n");
        assert!(w.path.join("sub/d.txt").is_file());
        assert!(!w.path.join("c.txt").exists(), "a deleted file is deleted there too");
        assert!(!w.path.join("build.log").exists(), "an ignored file is not carried");
        assert_eq!(std::fs::read_to_string(w.path.join("forced.log")).unwrap(), "forced\n", "a force-added one is");
        assert_eq!(git_ok(&main, &["ls-tree", "-r", &w.base, "vendor"]), "", "a nested repository is no gitlink");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal"]), "");
        let head = &before[2];
        assert_ne!(&w.base, head);
        assert_eq!(&git_ok(&main, &["rev-parse", &format!("{}^", w.base)]), head, "one commit on top of HEAD");
        assert_eq!(git_ok(&main, &["log", "-1", "--format=%s", &w.base]), format!("krowk: working state for {}", w.hex));
        assert_eq!(git_ok(&w.path, &["rev-parse", "HEAD"]), w.base);
        assert!(std::fs::read_dir(root.join(&w.repo_id)).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".index")), "the scratch index is gone");
        assert_eq!(finish(&w).unwrap(), Finished::Removed, "unchanged from the working state");
        assert!(!w.path.exists());

        // An edit is counted from the working state, not from HEAD.
        let w = create(&main, &root, "s2").unwrap();
        std::fs::write(w.path.join("a.txt"), "the child's\n").unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 0, dirty: true });
        assert_eq!(parent(&main), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A clean parent, one whose only change is ignored, or one with no
    /// index at all: the branch starts at HEAD, with no commit of krowk's.
    #[test]
    fn wt14_a_clean_parent_gives_a_branch_at_head() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("clean");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(w.base, head);
        assert_eq!(git_ok(&w.path, &["rev-parse", "HEAD"]), head);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        std::fs::write(main.join(".git/info/exclude"), "*.log\n").unwrap();
        std::fs::write(main.join("build.log"), "ignored\n").unwrap();
        let w = create(&main, &root, "s2").unwrap();
        assert_eq!(w.base, head);
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        // No index to copy: the scratch index starts from HEAD, and the
        // same files give HEAD again.
        std::fs::remove_file(main.join(".git/index")).unwrap();
        let w = create(&main, &root, "s3").unwrap();
        assert_eq!(w.base, head);
        assert!(!main.join(".git/index").exists(), "no index made for the parent");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Outside a repository: refused by name, nothing made.
    #[test]
    fn wt3_outside_a_repository_nothing_is_made() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let base = std::env::temp_dir().join(format!("krowk-worktree-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("plain")).unwrap();
        // GIT_CEILING_DIRECTORIES is not ours to set: a temp dir inside a
        // repository would make this one, so only judge when it is not.
        let r = create(&base.join("plain"), &base.join("worktrees"), "s");
        if krowk_api::git::query(&base).unwrap().args(["rev-parse", "--git-dir"]).output().is_ok_and(|o| !o.status.success()) {
            assert_eq!(r, Err(Error::NotARepository));
            assert!(!base.join("worktrees").exists());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repository with a submodule `mid` that has a submodule `inner`,
    /// both initialised in the main checkout, their sources at
    /// `<base>/mid` and `<base>/inner`.
    fn with_submodules(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (base, main, root) = repo(name);
        let source = |n: &str| {
            let d = base.join(n);
            std::fs::create_dir_all(&d).unwrap();
            git_ok(&d, &["init", "-q", "-b", "main"]);
            std::fs::write(d.join("f"), format!("{n}\n")).unwrap();
            git_ok(&d, &["add", "f"]);
            git_ok(&d, &["commit", "-q", "-m", n]);
            d
        };
        let (inner, mid) = (source("inner"), source("mid"));
        let add = |dir: &Path, from: &Path, path: &str| {
            git_ok(dir, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", from.to_str().unwrap(), path]);
            git_ok(dir, &["commit", "-q", "-m", path]);
        };
        add(&mid, &inner, "inner");
        // Hides `inner` from `mid`'s own status: a change in it must still
        // keep the worktree.
        git_ok(&mid, &["config", "--file", ".gitmodules", "submodule.inner.ignore", "all"]);
        git_ok(&mid, &["commit", "-q", "-am", "ignore inner"]);
        add(&main, &mid, "mid");
        git_ok(&main, &["-c", "protocol.file.allow=always", "submodule", "update", "--init", "--recursive", "-q"]);
        (base, main, root)
    }

    /// A submodule in a submodule, initialised in the main checkout: both
    /// populated in a new worktree at the recorded commits from the main
    /// checkout's copies, with their sources gone (no network), `origin`
    /// still their own URL. Removing the worktree leaves nothing of them.
    #[test]
    fn wt15_nested_submodules_come_from_the_main_checkout_without_the_network() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = with_submodules("submodules");
        let url = git_ok(&main, &["config", "--get", "submodule.mid.url"]);
        std::fs::rename(base.join("mid"), base.join("mid-gone")).unwrap();
        std::fs::rename(base.join("inner"), base.join("inner-gone")).unwrap();
        let config = std::fs::read(main.join(".git/config")).unwrap();
        let w = create(&main, &root, "s1").unwrap();
        assert!(std::fs::read_dir(w.path.join("mid")).unwrap().next().is_none(), "git worktree add leaves it empty");
        assert_eq!(prepare(&Prepare::new(&w, &WorktreesConfig::default())), Vec::<String>::new());
        assert_eq!(std::fs::read(main.join(".git/config")).unwrap(), config, "the shared config is not written");
        assert_eq!(std::fs::read_to_string(w.path.join("mid/f")).unwrap(), "mid\n");
        assert_eq!(std::fs::read_to_string(w.path.join("mid/inner/f")).unwrap(), "inner\n");
        let status = git_ok(&w.path, &["submodule", "status", "--recursive"]);
        assert_eq!(status.lines().count(), 2, "{status}");
        assert!(!status.lines().any(|l| l.starts_with(['-', '+', 'U'])), "both at the recorded commits: {status}");
        assert_eq!(status, git_ok(&main, &["submodule", "status", "--recursive"]));
        assert_eq!(git_ok(&w.path.join("mid"), &["config", "--get", "remote.origin.url"]), url, "origin is the submodule's own URL");
        let admin = PathBuf::from(git_ok(&w.path, &["rev-parse", "--absolute-git-dir"]));
        assert_eq!(PathBuf::from(git_ok(&w.path.join("mid/inner"), &["rev-parse", "--absolute-git-dir"])), admin.join("modules/mid/modules/inner"), "in the worktree's admin dir");
        assert_eq!(git_ok(&w.path, &["status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none"]), "");

        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        assert!(!w.path.exists() && !admin.exists());
        assert!(!w.common.join("worktrees").exists() || std::fs::read_dir(w.common.join("worktrees")).unwrap().next().is_none(), "no admin dir left");
        let modules: Vec<_> = std::fs::read_dir(w.common.join("modules")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(modules, ["mid"], "the main checkout's own, nothing more");
        assert_eq!(git_ok(&main, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count(), 1);
        assert_eq!(git_ok(&main, &["branch", "--list", "krowk/*"]), "");

        assert_eq!(std::fs::read(main.join(".git/config")).unwrap(), config);

        // An edit in `inner`, which `mid`'s `.gitmodules` ignores, is a
        // change: the worktree is kept, not force-removed.
        let w = create(&main, &root, "s2").unwrap();
        prepare(&Prepare::new(&w, &WorktreesConfig::default()));
        std::fs::write(w.path.join("mid/inner/f"), "changed\n").unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 0, dirty: true });
        assert_eq!(std::fs::read_to_string(w.path.join("mid/inner/f")).unwrap(), "changed\n");
        // So is a new file in `mid`, whose own config hides untracked files.
        let w = create(&main, &root, "s3").unwrap();
        prepare(&Prepare::new(&w, &WorktreesConfig::default()));
        git_ok(&w.path.join("mid"), &["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(w.path.join("mid/new.txt"), "new\n").unwrap();
        assert_eq!(finish(&w).unwrap(), Finished::Kept { commits: 0, dirty: true });
        assert!(w.path.join("mid/new.txt").is_file());
        assert_eq!(std::fs::read(main.join(".git/config")).unwrap(), config);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// No `.gitmodules`: the step runs no git and says nothing, even with a
    /// gitlink in the tree.
    #[test]
    fn wt15_without_gitmodules_nothing_runs() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = repo("no-gitmodules");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        git_ok(&main, &["update-index", "--add", "--cacheinfo", &format!("160000,{head},lib")]);
        git_ok(&main, &["commit", "-q", "-m", "a gitlink"]);
        let w = create(&main, &root, "s1").unwrap();
        assert_eq!(prepare(&Prepare::new(&w, &WorktreesConfig::default())), Vec::<String>::new());
        let mut failed = Vec::new();
        init_submodules(&w.path, Path::new("/nonexistent"), "", 0, &mut failed);
        assert!(failed.is_empty());
        assert!(std::fs::read_dir(w.path.join("lib")).map_or(true, |mut d| d.next().is_none()));
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A submodule that cannot be initialised leaves the worktree usable and
    /// a note naming it with git's words at the top of the agent's first
    /// prompt. Nothing `.gitmodules` names runs or is copied: an `ext::`
    /// URL is refused, and so is a local path the main checkout has not
    /// initialised.
    #[test]
    fn wt15_a_submodule_that_fails_is_a_note_in_the_first_prompt() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let (base, main, root) = with_submodules("submodule-fails");
        let marker = base.join("ran");
        let head = git_ok(&main, &["rev-parse", "HEAD"]);
        // Its empty directory, as a checkout leaves it: missing, it would be
        // a deletion in the parent's working state.
        std::fs::create_dir(main.join("evil")).unwrap();
        git_ok(&main, &["update-index", "--add", "--cacheinfo", &format!("160000,{head},evil")]);
        git_ok(&main, &["config", "--file", ".gitmodules", "submodule.evil.path", "evil"]);
        git_ok(&main, &["config", "--file", ".gitmodules", "submodule.evil.url", &format!("ext::sh -c touch% {}", marker.display())]);
        git_ok(&main, &["add", ".gitmodules"]);
        git_ok(&main, &["commit", "-q", "-m", "evil"]);
        // `mid` not initialised in the main checkout, its source a local
        // path that exists.
        git_ok(&main, &["submodule", "deinit", "-q", "-f", "mid"]);
        let config = std::fs::read(main.join(".git/config")).unwrap();
        let w = create(&main, &root, "s1").unwrap();
        let notes = prepare(&Prepare::new(&w, &WorktreesConfig::default()));
        assert_eq!(std::fs::read(main.join(".git/config")).unwrap(), config, "the deinitialised submodule is not brought back");
        assert_eq!(notes.len(), 1, "{notes:?}");
        let note = &notes[0];
        assert!(note.contains("- evil: git submodule update:") && note.contains("transport 'ext' not allowed"), "{note}");
        assert!(note.contains("- mid: git submodule update:") && note.contains("transport 'file' not allowed"), "{note}");
        assert!(!marker.exists(), "the ext:: command did not run");
        assert!(!w.path.join("mid/f").exists());
        let prompt = first_prompt(&notes, "Fix the build.");
        assert!(prompt.starts_with("Note from krowk, which prepared this worktree: these submodules could not be initialised"), "{prompt}");
        assert!(prompt.ends_with("\n\nFix the build."), "{prompt}");
        assert_eq!(first_prompt(&[], "Fix the build."), "Fix the build.");
        assert_eq!(finish(&w).unwrap(), Finished::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A note names at most twenty submodules, each in at most 300 bytes.
    #[test]
    fn wt15_the_note_is_bounded() {
        let failed: Vec<String> = (0..25).map(|i| format!("- s{i}: {}", "é".repeat(400))).collect();
        let note = failure_note(&failed);
        let lines: Vec<&str> = note.lines().collect();
        assert_eq!(lines.len(), 1 + 20 + 1, "{note}");
        assert!(lines[1..21].iter().all(|l| l.len() <= MAX_NOTE_LINE + "…".len() && l.ends_with('…')));
        assert_eq!(lines[21], "- and 5 more");
        assert_eq!(failure_note(&["- a: no".into()]), "these submodules could not be initialised and are empty directories here:\n- a: no");
    }

    /// Relative submodule URLs, as `git submodule init` resolves them.
    #[test]
    fn wt15_a_relative_url_is_resolved_against_origin() {
        assert_eq!(resolve_url("https://h/org/repo.git", "../lib.git"), "https://h/org/lib.git");
        assert_eq!(resolve_url("https://h/org/repo/", "./lib"), "https://h/org/repo/lib");
        assert_eq!(resolve_url("git@h:org/repo", "../../other/lib"), "git@h:other/lib");
        assert_eq!(resolve_url("git@h:org/repo", "../lib"), "git@h:org/lib");
    }

    #[test]
    fn wt3_the_repo_id_is_16_hex_digits_of_the_common_dirs_sha256() {
        let id = repo_id(Path::new("/r/.git"));
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(id, repo_id(Path::new("/s/.git")));
        let hex = random_hex();
        assert!(hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{hex}");
    }
}
