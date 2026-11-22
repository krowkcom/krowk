//! Handoff: a live session moved from the machine hosting it to another of
//! the person's (engineering/harness.md → Handoff).
//!
//! **The bundle** (`Bundle`, R-HAND-1) is what the log does not carry: the
//! commit the session's repository was at, a `git diff --binary` of its
//! uncommitted changes, the untracked files the session touched (changed
//! since it started), and for a backend session the vendor's transcript
//! (R-HAND-5). Dependencies and build state are not in it: the target
//! rebuilds them. The log itself the target reads from the registry, which
//! the source brings up to date with a checkpoint first.
//!
//! **It travels** as a transport artifact (R-HAND-2): sealed under a fresh
//! key (`e2e::seal_transport`) that reaches the target only inside the
//! session's E2E link, stored for a day or until the target spends it.
//!
//! **The target applies it** (`apply`) into a fresh worktree of its own
//! clone at the bundle's base, writes the log back (`restore_log`) and logs
//! `session.moved` with the worktree it now runs in.
//!
//! **A job and its result** (R-ALLOC-4, R-ALLOC-6) are bundles too. A job,
//! what a supervisor hands another machine with a `run` command, is the
//! bundle of the supervisor's directory, its untracked files as `pack`
//! takes them (`--exclude-standard`, links left out) but all of them, not
//! only those changed since a session started; plus `commits`, a git
//! bundle of the commits on the way to `base` that no remote has, so a
//! branch never pushed still arrives; plus the task's `prompt`, which so
//! never meets the 16 KB a command may be. At most `MAX_JOB` in all
//! (`pack_job`). The target applies it into a fresh worktree and records
//! the job's state there as a snapshot commit (`apply_job`). A result is
//! the bundle of the child's worktree against that snapshot, so it holds
//! the child's work alone, committed or not, as `diff` and `untracked`
//! (`pack_result`); the child's bridge, which holds its lease, stores it
//! when the one turn completes and logs `task.result`. The supervisor's
//! working tree already holds the job's state, so the result lands there
//! as a local worktree subagent's changes land (`apply_result`). Both
//! travel as transport artifacts like a handoff's; the target never
//! spends a job's, so a fallback machine can read the same one.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// The bundle's format.
pub const BUNDLE_V1: u32 = 1;

/// The most the untracked files together may be: past it the handoff is
/// refused, rather than shipping build output nobody meant to move.
pub const MAX_UNTRACKED: u64 = 64 << 20;

/// The most a job may be, its untracked files, diff and git bundle
/// together (R-ALLOC-4).
pub const MAX_JOB: u64 = 64 << 20;

/// What a job over `MAX_JOB` fails with.
pub const JOB_TOO_BIG: &str = "This task carries more than 64 MiB of unpushed work. Push it, or run it on this machine.";

/// What a job fails with when its unpushed commits change a fenced
/// directory or a submodule, which no other machine is handed.
pub const JOB_FENCED: &str = "This task's unpushed commits change .git, .claude, .codex, .krowk or a submodule, which krowk never hands to another machine. Push them, or run it on this machine.";

/// What a handoff carries beside the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    pub v: u32,
    pub session: String,
    /// The commit the session's repository was at.
    pub base: String,
    /// The session's directory under the repository's top, `""` at the top.
    pub subdir: String,
    /// `git diff --binary` of the uncommitted changes against `base`.
    #[serde(with = "b64")]
    pub diff: Vec<u8>,
    /// The untracked files changed since the session started.
    pub untracked: Vec<Untracked>,
    /// The vendor's transcript, for a session last on a backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Transcript>,
    /// A job's: `git bundle` of the commits reachable from `base` that no
    /// remote has, which the target fetches `base` from when its clone
    /// lacks it. Empty when every commit is on a remote, and in a handoff's
    /// or a result's bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "b64")]
    pub commits: Vec<u8>,
    /// A job's: the task, the child's one turn. Empty in a handoff's or a
    /// result's bundle.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Untracked {
    /// Relative to the repository's top, `/`-separated.
    pub path: String,
    pub executable: bool,
    #[serde(with = "b64")]
    pub bytes: Vec<u8>,
}

/// A backend's transcript: Claude Code's `<id>.jsonl`, placed on the
/// target where Claude Code looks for it from the new working directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    pub backend: String,
    pub vendor_session_id: String,
    #[serde(with = "b64")]
    pub bytes: Vec<u8>,
}

mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        base64::engine::general_purpose::STANDARD.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl Bundle {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("json")
    }

    pub fn decode(bytes: &[u8], session: &str) -> Result<Bundle, String> {
        let b: Bundle = serde_json::from_slice(bytes).map_err(|e| format!("the handoff bundle is not one this krowk reads: {e}"))?;
        if b.v > BUNDLE_V1 {
            return Err(format!("the handoff bundle is format {}, newer than this krowk reads — upgrade krowk", b.v));
        }
        if b.session != session {
            return Err(format!("the handoff bundle is session {}'s, not {session}'s", b.session));
        }
        Ok(b)
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = gruntas_api::git::query(dir).and_then(|mut c| c.args(args).output()).map_err(|e| format!("git {}: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_out(dir, args).map(|o| String::from_utf8_lossy(&o).trim().to_string())
}

/// The vendor transcript a session's log names last, when the session runs
/// on a backend: its backend, vendor session id and path.
pub fn last_backend(events: &[Value]) -> Option<(String, String, String)> {
    events.iter().rev().find(|e| e["type"] == "backend.session").and_then(|e| Some((e["backend"].as_str()?.to_string(), e["vendorSessionId"].as_str()?.to_string(), e["transcriptPath"].as_str()?.to_string())))
}

/// The instance the session's next turn runs on: the last model a turn ran
/// on or a switch moved it to.
pub fn last_instance(events: &[Value]) -> Option<String> {
    events.iter().rev().find_map(|e| match e["type"].as_str() {
        Some("turn.started") => e["model"]["instance"].as_str().map(String::from),
        Some("model.switched") => e["to"]["instance"].as_str().map(String::from),
        _ => None,
    })
}

/// The untracked files under `top` (`--exclude-standard`, links left out)
/// changed at or after `since_ms`, read whole, those in a fenced directory
/// left out when `fence`; `over(rel)` is the error when they come to more
/// than `limit` bytes together, found from the sizes before any file past
/// the limit is read.
fn collect_untracked(top: &Path, since_ms: i64, limit: u64, fence: bool, over: impl Fn(&str) -> String) -> Result<Vec<Untracked>, String> {
    let listed = git_out(top, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked = Vec::new();
    let mut total = 0u64;
    for rel in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let rel = String::from_utf8(rel.to_vec()).map_err(|_| "an untracked file's name is not UTF-8, so it cannot move".to_string())?;
        if fence && crate::permissions::scope::fenced_dir(Path::new(&rel)).is_some() {
            continue;
        }
        let path = top.join(&rel);
        let Ok(meta) = path.symlink_metadata() else { continue };
        // Links are left: what they point at is this machine's.
        if !meta.is_file() {
            continue;
        }
        let changed = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        if changed < since_ms {
            continue;
        }
        total += meta.len();
        if total > limit {
            return Err(over(&rel));
        }
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        untracked.push(Untracked { path: rel, executable, bytes });
    }
    Ok(untracked)
}

/// Whether `git log --raw -z` / `git diff --raw -z` output (`out`) names a
/// fenced path or a gitlink (mode 160000) on either side.
fn touches_fence(out: &[u8]) -> bool {
    let mut tokens = out.split(|b| *b == 0).map(String::from_utf8_lossy);
    while let Some(t) = tokens.next() {
        if !t.starts_with(':') {
            continue;
        }
        let mut f = t[1..].split(' ');
        if f.next() == Some("160000") || f.next() == Some("160000") {
            return true;
        }
        if tokens.next().is_some_and(|path| crate::permissions::scope::fenced_dir(Path::new(path.as_ref())).is_some()) {
            return true;
        }
    }
    false
}

/// Whether the unpushed commits on the way to `head` (those no remote ref
/// of `repo` reaches) change a fenced path or a gitlink.
fn commits_touch_fence(repo: &Path, head: &str) -> Result<bool, String> {
    let raw = git_out(repo, &["log", "-m", "--raw", "-z", "--format=", "--no-renames", "--no-ext-diff", head, "--not", "--remotes"])?;
    Ok(touches_fence(&raw))
}

/// The source's half: the bundle of the session running in `cwd`, started
/// at `since_ms`. `transcript` is the vendor's file the log names, read
/// only when it is one: a regular file named for its session, not a link.
pub fn pack(cwd: &Path, session: &str, since_ms: i64, transcript: Option<(&str, &str, &Path)>) -> Result<Bundle, String> {
    let top = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| format!("{} is not in a git repository, so its work cannot move ({e})", cwd.display()))?);
    let base = git_text(&top, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(|_| "the repository has no commit yet to move from".to_string())?;
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let subdir = canon(cwd).strip_prefix(canon(&top)).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let diff = git_out(&top, &["diff", "--binary", "--no-ext-diff", "--no-textconv", "--no-renames", "HEAD", "--"])?;
    let untracked = collect_untracked(&top, since_ms, MAX_UNTRACKED, false, |rel| format!("the untracked files the session changed come to more than {} MB — commit or ignore what should not move (it was {rel} that went over)", MAX_UNTRACKED >> 20))?;
    let transcript = match transcript {
        Some((backend, id, path)) if crate::handoff::valid_session_id(id) && path.file_name().and_then(|n| n.to_str()) == Some(&format!("{id}.jsonl")) && path.symlink_metadata().is_ok_and(|m| m.is_file()) => {
            Some(Transcript { backend: backend.to_string(), vendor_session_id: id.to_string(), bytes: std::fs::read(path).map_err(|e| format!("read the transcript {}: {e}", path.display()))? })
        }
        _ => None,
    };
    Ok(Bundle { v: BUNDLE_V1, session: session.to_string(), base, subdir, diff, untracked, transcript, commits: Vec::new(), prompt: String::new() })
}

/// The supervisor's half of a job (R-ALLOC-4): the bundle of `cwd`'s
/// repository at HEAD, its uncommitted changes, every untracked file `pack`
/// would take, the commits no remote has, and `prompt`, for the transport
/// stored under `session`. Left out: any change inside a `.git`,
/// `.claude`, `.codex` or `.krowk` directory (`crate::permissions::scope::fenced_dir`,
/// as a local return fences them) and any change to a submodule, which
/// would name what runs, or what is allowed, on the target. The unpushed
/// commits are held to the same fence: when the commits on the way to
/// HEAD that no remote has change a fenced path or a gitlink, it fails at
/// once with `JOB_FENCED`, since a commit cannot be carried in part. Fails
/// with `JOB_TOO_BIG` past `MAX_JOB`, `pack`'s own bound on untracked files
/// included.
pub fn pack_job(cwd: &Path, session: &str, prompt: &str) -> Result<Bundle, String> {
    let top = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| format!("{} is not in a git repository, so its work cannot move ({e})", cwd.display()))?);
    let base = git_text(&top, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(|_| "the repository has no commit yet to move from".to_string())?;
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let subdir = canon(cwd).strip_prefix(canon(&top)).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    if commits_touch_fence(&top, &base)? {
        return Err(JOB_FENCED.to_string());
    }
    // The uncommitted changes, less those in a fenced directory and in
    // submodules.
    let changed = git_out(&top, &["diff", "--name-only", "-z", "--no-renames", "--ignore-submodules=all", "HEAD", "--"])?;
    let mut fenced = Vec::new();
    for name in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let Ok(name) = std::str::from_utf8(name) else {
            return Err(format!("a changed file's name is not UTF-8, so it cannot move. {JOB_FENCED}"));
        };
        if crate::permissions::scope::fenced_dir(Path::new(name)).is_some() {
            fenced.push(format!(":(exclude,literal){name}"));
        }
    }
    // One argument each: past what a command line takes, the job is not made.
    if fenced.len() > 1000 {
        return Err(JOB_FENCED.to_string());
    }
    let mut args = vec!["diff", "--binary", "--no-ext-diff", "--no-textconv", "--no-renames", "--ignore-submodules=all", "HEAD", "--"];
    args.extend(fenced.iter().map(String::as_str));
    let diff = git_out(&top, &args)?;
    let mut left = MAX_JOB.checked_sub((diff.len() + prompt.len()) as u64).ok_or_else(|| JOB_TOO_BIG.to_string())?;
    let untracked = collect_untracked(&top, 0, left, true, |_| JOB_TOO_BIG.to_string())?;
    left = left.saturating_sub(untracked.iter().map(|f| f.bytes.len() as u64).sum::<u64>());
    // Commits no remote has: none when every one is on a remote, which `git
    // bundle` would refuse to make an empty bundle of.
    let unpushed = git_text(&top, &["rev-list", "--count", "HEAD", "--not", "--remotes"])?;
    let commits = if unpushed == "0" { Vec::new() } else { git_run(&top, &["bundle", "create", "-", "HEAD", "--not", "--remotes"], None)? };
    if commits.len() as u64 > left {
        return Err(JOB_TOO_BIG.to_string());
    }
    Ok(Bundle { v: BUNDLE_V1, session: session.to_string(), base, subdir, diff, untracked, transcript: None, commits, prompt: prompt.to_string() })
}

/// A job applied on the target: where, and the snapshot commit of its
/// state (base, uncommitted changes and untracked files) the result is
/// packed against.
#[derive(Debug)]
pub struct JobApplied {
    pub applied: Applied,
    pub snapshot: String,
}

/// The target's half of a job: as `apply`, a fresh worktree of `repo` at
/// the job's base with its changes, and the state recorded as a snapshot
/// commit. Refused (`bad_job` to the supervisor) for a `base` that is not
/// a commit id in hex, or a job changing anything `pack_job` leaves out (a
/// fenced directory, a submodule), whether in its diff, its untracked
/// files or its commits: once `base` is fetched, `git diff --name-only`
/// from the newest ancestor of `base` an `origin` ref reaches to `base`
/// must touch no fenced path and no gitlink. Such a job came from no
/// krowk's `pack_job`.
/// `base` is fetched, when the clone lacks it, from `commits` into a ref
/// of krowk's own (`refs/krowk/jobs/…`), never a branch or tag the person
/// has, then from origin. The snapshot is written with krowk's own
/// identity (`-c user.name=krowk -c user.email=krowk@localhost`) from the
/// worktree's files, whatever the person's git config and the worktree's
/// index say.
pub fn apply_job(b: &Bundle, repo: &Path, worktrees: &Path) -> Result<JobApplied, String> {
    git_text(repo, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{} is not in a git repository", repo.display()))?;
    // Everything here came from another machine: a full commit id, nothing
    // git could read as an option or a name, and nothing pack_job leaves out.
    if !matches!(b.base.len(), 40 | 64) || !b.base.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("the job names {:?} as its base, which is not a commit id", b.base));
    }
    if let Some(f) = b.untracked.iter().find(|f| crate::permissions::scope::fenced_dir(Path::new(&f.path)).is_some()) {
        return Err(format!("the job carries {}, inside a directory krowk never hands to another machine", f.path));
    }
    if !b.diff.is_empty() {
        let text = String::from_utf8_lossy(&b.diff);
        // pack_job asks for no renames, so a patch with one or a copy is not its: the old name is not in `--numstat`.
        if text.lines().any(|l| l.starts_with("rename ") || l.starts_with("copy ") || l.starts_with("similarity ") || l.starts_with("dissimilarity ")) {
            return Err("the job carries a rename or a copy, which krowk's jobs never do".to_string());
        }
        let names = git_run(repo, &["apply", "--numstat", "-z", "-"], Some(&b.diff)).map_err(|e| format!("the job's changes are not a patch: {e}"))?;
        // `added<TAB>deleted<TAB>path`, NUL-ended.
        for t in names.split(|c| *c == 0).filter(|p| !p.is_empty()) {
            let t = String::from_utf8_lossy(t);
            if t.splitn(3, '\t').nth(2).is_none_or(|p| crate::permissions::scope::fenced_dir(Path::new(p)).is_some()) {
                return Err("the job carries changes inside a directory krowk never hands to another machine".to_string());
            }
        }
        if text.lines().any(|l| l.trim_end() == "new file mode 160000" || l.starts_with("+Subproject commit ") || l.starts_with("-Subproject commit ") || l.ends_with(" 160000")) {
            return Err("the job carries a submodule change, which krowk never hands to another machine".to_string());
        }
    }
    let commit = format!("{}^{{commit}}", b.base);
    let has = || git_text(repo, &["cat-file", "-e", &commit]).is_ok();
    let fetch_origin = || {
        let _ = gruntas_api::git::command(repo).and_then(|mut c| c.env("GIT_TERMINAL_PROMPT", "0").args(["fetch", "--quiet", "origin"]).output());
    };
    // The job's own commits first: read into the object store, and held by a
    // ref of krowk's own (never a branch or tag the person has). They stand
    // on commits a remote has, which the clone may lack: then origin is
    // fetched and the bundle read again.
    let job_ref = format!("refs/krowk/jobs/{}", b.base);
    let unbundle = || {
        if !b.commits.is_empty() && git_run(repo, &["-c", "transfer.fsckObjects=true", "bundle", "unbundle", "-"], Some(&b.commits)).is_ok() && has() {
            let _ = git_run(repo, &["update-ref", &job_ref, &b.base], None);
        }
    };
    if !has() {
        unbundle();
    }
    if !has() {
        fetch_origin();
        unbundle();
    }
    if !has() {
        return Err(format!("this clone does not have commit {} the job starts from, and neither the job's commits nor `git fetch origin` brought it — push it, or run the task on the machine that has it", b.base));
    }
    if commits_touch_fence(repo, &b.base)? {
        let _ = git_run(repo, &["update-ref", "-d", &job_ref], None);
        return Err("the job's commits change a directory or submodule krowk never hands to another machine".to_string());
    }
    let applied = apply(b, repo, worktrees)?;
    // The state recorded as a commit on top of the base, written from the
    // worktree's files: everything staged (the worktree is this job's own),
    // krowk's own identity. HEAD stays the base, so the job's changes show
    // as the work in progress they are.
    let dir = &applied.worktree.path;
    let snapshot = (|| {
        git_run(dir, &["add", "-A"], None)?;
        let tree = String::from_utf8_lossy(&git_run(dir, &["write-tree"], None)?).trim().to_string();
        let out = git_run(dir, &["-c", "user.name=krowk", "-c", "user.email=krowk@localhost", "commit-tree", &tree, "-p", &b.base, "-m", "krowk: the job's state"], None)?;
        Ok::<_, String>(String::from_utf8_lossy(&out).trim().to_string())
    })();
    match snapshot {
        Ok(snapshot) => Ok(JobApplied { applied, snapshot }),
        Err(e) => {
            let _ = crate::worktree::discard(&applied.worktree);
            Err(format!("the job's state could not be recorded: {e}"))
        }
    }
}

/// What a result over `MAX_JOB` fails with.
pub const RESULT_TOO_BIG: &str = "The child's changes come to more than 64 MiB, so they cannot be returned. It stays in its worktree on the machine that ran it.";

/// What a result refuses, as the worktree's config, which the child could
/// have written, names a place or a program git would go to.
pub const RESULT_CONFIG: &str = "The child's worktree sets core.worktree or includes another config file, so its changes were not read. It stays in its worktree on the machine that ran it.";

/// The child's result (R-ALLOC-6): everything the worktree at `worktree`
/// changed against `snapshot`, the job's state, committed or not, new
/// files included, for the transport stored under the child's `session`;
/// `base` is `snapshot`. Empty when the child changed nothing. The changes
/// are all in `diff`: the worktree's files, whatever git ignores left out,
/// taken into an index of its own (a name no other pack shares) and compared
/// with the snapshot's tree, so neither the child's index nor its commits
/// decide what is the child's work, and the job's own files, which the
/// snapshot holds, are not in it.
///
/// The child wrote the worktree, its config and attributes included, so git
/// runs here as it never would on a person's repository: no system or
/// global config, no attributes outside the tree, and every filter the
/// repository's config names switched off (no `clean` or `process` command
/// runs), `core.worktree` and config includes refused. Past `MAX_JOB` the
/// files it would take are not hashed, or the diff not read on; a gitlink
/// (a submodule, or a repository inside the tree) is refused.
pub fn pack_result(worktree: &Path, snapshot: &str, session: &str) -> Result<Bundle, String> {
    pack_result_within(worktree, snapshot, session, MAX_JOB)
}

/// `pack_result` with `cap` as the most it may be (`MAX_JOB`).
fn pack_result_within(worktree: &Path, snapshot: &str, session: &str, cap: u64) -> Result<Bundle, String> {
    if !(snapshot.len() == 40 || snapshot.len() == 64) || !snapshot.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{snapshot:?} is not a commit id"));
    }
    let sealed = |c: &mut std::process::Command| {
        c.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_ATTR_NOSYSTEM", "1");
        for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"] {
            c.env_remove(var);
        }
        c.args(["-c", "core.attributesFile=/dev/null", "-c", "core.excludesFile=/dev/null", "-c", "core.splitIndex=false"]);
    };
    let query = |dir: &Path, args: &[&str]| -> Result<String, String> {
        let out = gruntas_api::git::query(dir).and_then(|mut c| {
            sealed(&mut c);
            c.args(args).output()
        }).map_err(|e| format!("git {}: {e}", args[0]))?;
        if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { Err(format!("git {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim())) }
    };
    let config = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = gruntas_api::git::query(worktree).and_then(|mut c| {
            sealed(&mut c);
            c.args(["config", "--no-includes", "-z"]).args(args).output()
        }).map_err(|e| format!("git config: {e}"))?;
        // 1 is no match; a config git cannot read is read as the worst.
        match out.status.code() {
            Some(0 | 1) => Ok(out.stdout),
            _ => Err(RESULT_CONFIG.to_string()),
        }
    };
    if !config(&["--get-regexp", r"^(core\.worktree|include\.path|includeif\..*\.path)$"])?.is_empty() {
        return Err(RESULT_CONFIG.to_string());
    }
    let top = PathBuf::from(query(worktree, &["rev-parse", "--show-toplevel"]).map_err(|e| format!("{} is not in a git repository ({e})", worktree.display()))?);
    let gitdir = PathBuf::from(query(&top, &["rev-parse", "--absolute-git-dir"])?);
    // Its own git directory: a `.git` that is a link, or a file pointing
    // outside the repository's `worktrees`, is somewhere the child chose.
    let canon = |p: &Path| p.canonicalize().map_err(|e| format!("{}: {e}", p.display()));
    let dot_git = top.join(".git");
    let own = match std::fs::symlink_metadata(&dot_git) {
        Ok(m) if m.is_dir() => canon(&gitdir)? == canon(&dot_git)?,
        Ok(m) if m.is_file() => {
            let common = PathBuf::from(query(&top, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
            canon(&gitdir)?.starts_with(canon(&common)?.join("worktrees"))
        }
        _ => false,
    };
    if !own {
        return Err(RESULT_CONFIG.to_string());
    }
    let index = gitdir.join(format!("krowk-result-index-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    // Every filter the config names, switched off: an empty command is none.
    let mut off: Vec<String> = Vec::new();
    for entry in config(&["--get-regexp", r"^filter\."])?.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let key = String::from_utf8_lossy(entry.split(|b| *b == b'\n').next().unwrap_or_default()).to_string();
        if let Some(name) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.')).map(|(n, _)| n.to_string()) && !off.contains(&name) {
            off.push(name);
        }
    }
    let git = |args: &[&str]| -> std::process::Command {
        let mut c = gruntas_api::git::command(&top).expect("git command");
        sealed(&mut c);
        c.env("GIT_INDEX_FILE", &index);
        for n in &off {
            for what in ["clean", "process", "smudge"] {
                c.arg("-c").arg(format!("filter.{n}.{what}="));
            }
            c.arg("-c").arg(format!("filter.{n}.required=false"));
        }
        c.args(args);
        c
    };
    let run = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = git(args).output().map_err(|e| format!("git {}: {e}", args[0]))?;
        if out.status.success() { Ok(out.stdout) } else { Err(format!("git {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim())) }
    };
    let pack = || -> Result<Vec<u8>, String> {
        // What `add` would hash, counted before it does: the files the
        // child's own index has as changed or new, not every tracked one.
        let mut total = 0u64;
        let listed = {
            let mut c = gruntas_api::git::query(&top).map_err(|e| e.to_string())?;
            sealed(&mut c);
            let out = c.args(["ls-files", "-z", "--others", "--modified", "--exclude-standard"]).output().map_err(|e| format!("git ls-files: {e}"))?;
            if !out.status.success() {
                return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            out.stdout
        };
        for rel in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            if let Ok(m) = std::fs::symlink_metadata(top.join(String::from_utf8_lossy(rel).as_ref())) && m.is_file() {
                total += m.len();
            }
            if total > cap {
                return Err(RESULT_TOO_BIG.to_string());
            }
        }
        let tree = format!("{snapshot}^{{tree}}");
        run(&["read-tree", &tree])?;
        run(&["add", "--all", "--", ":/"])?;
        let now = String::from_utf8_lossy(&run(&["write-tree"])?).trim().to_string();
        let raw = run(&["diff-tree", "-r", "--raw", "-z", "--no-renames", &tree, &now])?;
        if raw.split(|b| *b == 0).any(|t| t.first() == Some(&b':') && String::from_utf8_lossy(t).split_whitespace().take(2).any(|m| m == ":160000" || m == "160000")) {
            return Err("The child added or changed a submodule, or a repository inside its tree, which a result cannot carry.".to_string());
        }
        let mut c = git(&["diff-tree", "-p", "--binary", "--full-index", "--no-ext-diff", "--no-textconv", "--no-renames", &tree, &now]);
        capped(&mut c, cap)
    };
    let diff = pack();
    let _ = std::fs::remove_file(&index);
    Ok(Bundle { v: BUNDLE_V1, session: session.to_string(), base: snapshot.to_string(), subdir: String::new(), diff: diff?, untracked: Vec::new(), transcript: None, commits: Vec::new(), prompt: String::new() })
}

/// `c`'s stdout, read no further than `cap` bytes: git is stopped there and
/// `RESULT_TOO_BIG` said.
fn capped(c: &mut std::process::Command, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut child = c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| format!("git diff-tree: {e}"))?;
    let mut stderr = child.stderr.take().expect("piped");
    let said = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stderr).take(64 << 10).read_to_string(&mut text);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        text
    });
    let mut out = Vec::new();
    let read = child.stdout.take().expect("piped").take(cap + 1).read_to_end(&mut out);
    if out.len() as u64 > cap {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RESULT_TOO_BIG.to_string());
    }
    read.map_err(|e| format!("git diff-tree: {e}"))?;
    let status = child.wait().map_err(|e| format!("git diff-tree: {e}"))?;
    let said = said.join().unwrap_or_default();
    if status.success() { Ok(out) } else { Err(format!("git diff-tree: {}", said.trim())) }
}

/// The supervisor lands `result` of `job` (R-ALLOC-8, D10). `to`'s working
/// tree holds the job's state, so the result applies on it: through a
/// worktree of `to`'s repository, under `worktrees`, at the job's state
/// (the job applied and committed there) with the result on top, whose
/// changes then go to `to`'s working tree as a local subagent's do
/// (`Approval::Agent`: never index or HEAD, whole or not at all, kept and
/// named on conflict, fenced paths and submodules kept). The worktree, and
/// what came of it. Slice 07 builds it.
pub fn apply_result(job: &Bundle, result: &Bundle, to: &Path, worktrees: &Path) -> Result<(crate::worktree::Worktree, crate::worktree::apply::Applied), String> {
    use crate::worktree::apply::Applied as Landed;
    let applied = apply(job, to, worktrees)?;
    let held = applied.held;
    let at = applied.worktree;
    let made = at.clone();
    let undo = |e: String| {
        let _ = crate::worktree::discard(&made);
        e
    };
    // The job's state, committed: what the result is the changes against.
    let dirty = git_text(&at.path, &["status", "--porcelain", "--untracked-files=normal"]).map_err(&undo)?;
    if !dirty.is_empty() {
        let mut add = gruntas_api::git::command(&at.path).map_err(|e| undo(e.to_string()))?;
        let out = add.args(["add", "--all", "--", ":/"]).output().map_err(|e| undo(format!("git add: {e}")))?;
        if !out.status.success() {
            return Err(undo(format!("git add: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        let mut commit = gruntas_api::git::command(&at.path).map_err(|e| undo(e.to_string()))?;
        let out = commit.args(["-c", "user.name=krowk", "-c", "user.email=krowk@localhost", "commit", "--quiet", "--no-verify", "--no-gpg-sign", "-m", "krowk: the job's state"]).output().map_err(|e| undo(format!("git commit: {e}")))?;
        if !out.status.success() {
            return Err(undo(format!("git commit: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
    }
    let head = git_text(&at.path, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(&undo)?;
    let wt = crate::worktree::Worktree { base: head, ..at };
    if !result.diff.is_empty() {
        apply_patch(&wt.path, &result.diff).map_err(|e| undo(format!("the child's changes did not apply on the job's state: {e}")))?;
    }
    for f in &result.untracked {
        write_untracked(&wt.path, f).map_err(&undo)?;
    }
    let landed = crate::worktree::finish_into(&wt, to).map_err(|e| format!("the child's changes could not be applied: {e}"));
    drop(held);
    Ok((wt, landed?.unwrap_or(Landed::Applied(Vec::new()))))
}

/// Where a session handed over landed: its worktree and the directory its
/// turns run in.
#[derive(Debug)]
pub struct Applied {
    pub worktree: crate::worktree::Worktree,
    pub held: crate::worktree::manage::Held,
    pub cwd: PathBuf,
}

/// A relative path that stays inside the tree it is joined to: only plain
/// names, and none of them `.git`.
fn inside(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) if !n.eq_ignore_ascii_case(".git") => out.push(n),
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// The target's half: a fresh worktree of the repository `repo` is in, at
/// the bundle's base — fetched first when this clone lacks it — with the
/// uncommitted changes applied and the untracked files written.
pub fn apply(b: &Bundle, repo: &Path, worktrees: &Path) -> Result<Applied, String> {
    git_text(repo, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{} is not in a git repository — run `krowk sync take` from a clone of the session's repository", repo.display()))?;
    let commit = format!("{}^{{commit}}", b.base);
    if git_text(repo, &["cat-file", "-e", &commit]).is_err() {
        let _ = gruntas_api::git::command(repo).and_then(|mut c| c.env("GIT_TERMINAL_PROMPT", "0").args(["fetch", "--quiet", "origin"]).output());
        git_text(repo, &["cat-file", "-e", &commit]).map_err(|_| format!("this clone does not have commit {} the session was at, and `git fetch origin` did not bring it — push it from the other machine, or run `krowk sync take` from a clone that has it", b.base))?;
    }
    let (worktree, held) = crate::worktree::create_at(repo, worktrees, &b.session, &b.base).map_err(|e| format!("the session's worktree could not be made: {e}"))?;
    let undo = |e: String| {
        let _ = crate::worktree::discard(&worktree);
        e
    };
    if !b.diff.is_empty() {
        apply_patch(&worktree.path, &b.diff).map_err(|e| undo(format!("the uncommitted changes did not apply at {}: {e}", b.base)))?;
    }
    for f in &b.untracked {
        write_untracked(&worktree.path, f).map_err(undo)?;
    }
    let cwd = match inside(&b.subdir) {
        Some(sub) => worktree.path.join(sub),
        None => worktree.path.clone(),
    };
    // As Claude Code resolves it, which its transcript's place is named for.
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    Ok(Applied { worktree, held, cwd })
}

/// `git args` in `dir` (hooks off, `git::command`), `input` fed on stdin:
/// its stdout.
fn git_run(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    let mut c = gruntas_api::git::command(dir).map_err(|e| e.to_string())?;
    let mut child = c.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("piped");
    let (_, out) = std::thread::scope(|s| {
        let w = s.spawn(move || input.map_or(Ok(()), |i| stdin.write_all(i)));
        let out = child.wait_with_output();
        (w.join().unwrap_or(Ok(())), out)
    });
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() { Ok(out.stdout) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// `git apply --binary` of `patch` in `dir`, fed on stdin.
fn apply_patch(dir: &Path, patch: &[u8]) -> Result<(), String> {
    git_run(dir, &["apply", "--binary", "--whitespace=nowarn", "-"], Some(patch)).map(|_| ())
}

/// One untracked file, written only inside the worktree: never over a file
/// there, never through a link on the way down.
fn write_untracked(top: &Path, f: &Untracked) -> Result<(), String> {
    let rel = inside(&f.path).ok_or_else(|| format!("the bundle names a file outside the worktree ({:?}), so it was not applied", f.path))?;
    let mut at = top.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for c in &parts[..parts.len() - 1] {
        at.push(c);
        match at.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("{} is not a directory of the worktree's own, so {} was not written", at.display(), f.path)),
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&at).map_err(|e| format!("create {}: {e}", at.display()))?,
        }
    }
    let path = top.join(&rel);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, if f.executable { 0o755 } else { 0o644 });
    use std::io::Write as _;
    o.open(&path).and_then(|mut w| w.write_all(&f.bytes)).map_err(|e| format!("write {}: {e}", path.display()))
}

/// The transcript placed where Claude Code finds it when resumed from `cwd`
/// with config directory `home`: answers where, or None for none carried.
pub fn place_transcript(b: &Bundle, home: &Path, cwd: &Path) -> Result<Option<PathBuf>, String> {
    let Some(t) = &b.transcript else { return Ok(None) };
    if t.backend != crate::claude::BACKEND || !crate::handoff::valid_session_id(&t.vendor_session_id) {
        return Ok(None);
    }
    let slug: String = cwd.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let dir = home.join("projects").join(slug);
    let mut at = home.to_path_buf();
    for p in ["projects", dir.file_name().and_then(|n| n.to_str()).unwrap_or_default()] {
        at.push(p);
        if at.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{} is a symbolic link, and krowk writes only into the account's own directories", at.display()));
        }
    }
    let mut d = std::fs::DirBuilder::new();
    d.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut d, 0o700);
    d.create(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.jsonl", t.vendor_session_id));
    write_private(&path, &t.bytes)?;
    Ok(Some(path))
}

/// The session's log on this machine, as the registry holds it — each event
/// once, in order — with `session.moved` appended: the turns that follow
/// run in `cwd`. A log here holding an event the registry's does not is
/// another history, and refused rather than replaced.
pub fn restore_log(sessions: &Path, id: &str, events: &[Value], cwd: &Path, device: &str, from_device: &str) -> Result<(), String> {
    use crate::log::{SessionLog, CONTEXT_FILE, EVENTS_FILE};
    check_local_log(sessions, id, events)?;
    let mut seen = std::collections::HashSet::new();
    let events: Vec<&Value> = events.iter().filter(|e| e["id"].as_str().is_some_and(|i| seen.insert(i.to_string()))).collect();
    let dir = sessions.join(id);
    let file = dir.join(EVENTS_FILE);
    let mut d = std::fs::DirBuilder::new();
    d.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut d, 0o700);
    d.create(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let jsonl: String = events.iter().map(|e| format!("{e}\n")).collect();
    write_private(&file, jsonl.as_bytes())?;
    if !dir.join(CONTEXT_FILE).exists() {
        write_private(&dir.join(CONTEXT_FILE), b"")?;
    }
    let (mut log, _) = SessionLog::open(sessions, id).map_err(|e| e.message().to_string())?;
    log.append(crate::protocol::LogBody::SessionMoved { cwd: cwd.display().to_string(), device: device.to_string(), from_device: from_device.to_string() }).map_err(|e| e.message().to_string())?;
    Ok(())
}

/// Fetches the images the session's events name into this machine's copy of
/// the session (R-IMG-12), so the first turn here sends them as the old host
/// would have. A log reference with no sha256 (written before images were
/// named by it) can't be fetched and is passed over, as it always was. An
/// image the registry can't give fails the move: better no turn than one
/// that tells the model its image is gone.
pub fn fetch_images(client: &gruntas_api::Client, keys: &gruntas_client::e2e::SessionKeys, id: &str, session_dir: &Path, events: &[Value]) -> Result<(), String> {
    let refs = events.iter().filter_map(|e| e.pointer("/item/images")).filter_map(|i| serde_json::from_value::<Vec<crate::protocol::ImageRef>>(i.clone()).ok()).flatten();
    for r in refs.filter(|r| r.sha256.is_some()) {
        match super::store::fetch_image(client, keys, id, session_dir, &r) {
            Ok(true) => {}
            Ok(false) => return Err(format!("image #{} ({}) of session {id} is not in the registry yet — the session's lease was given back; host it again where it was", r.number, r.file)),
            Err(e) => return Err(format!("could not fetch image #{} ({}) of session {id}: {e} — the session's lease was given back; host it again where it was", r.number, r.file)),
        }
    }
    Ok(())
}

/// Refuses a log of the session this machine holds with an event `events`,
/// the synced log, does not: it went on here after it left, and taking the
/// session would replace that history. Asked before anything moves.
pub fn check_local_log(sessions: &Path, id: &str, events: &[Value]) -> Result<(), String> {
    let Ok(here) = std::fs::read_to_string(sessions.join(id).join(crate::log::EVENTS_FILE)) else { return Ok(()) };
    let synced: std::collections::HashSet<&str> = events.iter().filter_map(|e| e["id"].as_str()).collect();
    match here.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|e| e["id"].as_str().map(String::from)).find(|i| !synced.contains(i.as_str())) {
        Some(extra) => Err(format!("this machine's log of session {id} holds event {extra}, which the synced log does not — it went on here after it left; nothing was moved")),
        None => Ok(()),
    }
}

/// A file written whole, 0600, through a temporary beside it and a rename.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

/// The key a bundle is sealed under, hex, as the host sends it.
pub fn key_hex(key: &[u8; 32]) -> String {
    gruntas_client::e2e::hex(key)
}

pub fn key_from_hex(hex: &str) -> Option<[u8; 32]> {
    gruntas_client::e2e::unhex(hex).and_then(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-HAND-1: nothing in a bundle lands outside the worktree or in its
    /// `.git`.
    #[test]
    fn r_hand_1_an_untracked_path_stays_inside_the_worktree() {
        assert_eq!(inside("src/new.rs"), Some(PathBuf::from("src/new.rs")));
        for bad in ["", "/etc/passwd", "../x", "a/../../x", ".git/hooks/pre-commit", "sub/.GIT/config", "./"] {
            assert_eq!(inside(bad), None, "{bad}");
        }
    }

    /// The bundle reads back as it went in, and refuses a newer format or
    /// another session's.
    #[test]
    fn r_hand_1_a_bundle_round_trips_and_names_its_session() {
        let b = Bundle { v: BUNDLE_V1, session: "s".into(), base: "abc".into(), subdir: "".into(), diff: vec![0, 1, 255], untracked: vec![Untracked { path: "a".into(), executable: true, bytes: b"x".to_vec() }], transcript: None, commits: Vec::new(), prompt: String::new() };
        assert_eq!(Bundle::decode(&b.encode(), "s").unwrap(), b);
        assert!(Bundle::decode(&b.encode(), "t").is_err());
        let text = String::from_utf8(b.encode()).unwrap();
        assert!(!text.contains("commits") && !text.contains("prompt"), "a handoff's bundle is written as before");
        let newer = Bundle { v: BUNDLE_V1 + 1, ..b };
        assert!(Bundle::decode(&newer.encode(), "s").unwrap_err().contains("upgrade"));
    }

    /// R-ALLOC-4: a job is a bundle with its unpushed commits and its
    /// prompt, and reads back as it went in; a bundle written before jobs
    /// reads as one with neither.
    #[test]
    fn r_alloc_4_a_job_round_trips_with_its_commits_and_prompt() {
        let job = Bundle { v: BUNDLE_V1, session: "s".into(), base: "abc".into(), subdir: "sub".into(), diff: Vec::new(), untracked: Vec::new(), transcript: None, commits: b"# v2 git bundle\n\0\xff".to_vec(), prompt: "x".repeat(64 * 1024) };
        assert_eq!(Bundle::decode(&job.encode(), "s").unwrap(), job);
        let old = Bundle::decode(br#"{"v":1,"session":"s","base":"abc","subdir":"","diff":"","untracked":[]}"#, "s").unwrap();
        assert!(old.commits.is_empty() && old.prompt.is_empty());
    }

    /// R-ALLOC-6: the event a child's log names its result with reads back
    /// as it went in, and an unchanged child's names no transport.
    #[test]
    fn r_alloc_6_a_task_result_event_round_trips() {
        use crate::protocol::LogBody;
        let ev = LogBody::TaskResult { turn_id: "t".into(), transport: Some("art_1".into()), key: Some("ab".repeat(32)), against: "abc".into(), denied: vec!["bash: rm -rf build".into()], error: None };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!((json["type"].as_str(), json["turnId"].as_str(), json["against"].as_str()), (Some("task.result"), Some("t"), Some("abc")));
        assert_eq!(serde_json::from_value::<LogBody>(json).unwrap(), ev);
        let unchanged: LogBody = serde_json::from_str(r#"{"type":"task.result","turnId":"t","against":"abc"}"#).unwrap();
        assert_eq!(unchanged, LogBody::TaskResult { turn_id: "t".into(), transport: None, key: None, against: "abc".into(), denied: Vec::new(), error: None });
    }

    /// A scratch directory of its own for `name`, empty.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("krowk-alloc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// `git args` in `dir`, which must succeed: its output, trimmed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare origin with one commit, under `root`, and a clone of it at
    /// `root/<clone>`.
    fn origin_and_clone(root: &Path, clone: &str) -> PathBuf {
        let seed = root.join("seed");
        if !seed.exists() {
            std::fs::create_dir_all(&seed).unwrap();
            git(&seed, &["init", "-q"]);
            std::fs::write(seed.join("kept.txt"), "base\n").unwrap();
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-q", "-m", "base"]);
            git(root, &["clone", "-q", "--bare", "seed", "origin.git"]);
        }
        git(root, &["clone", "-q", "origin.git", clone]);
        root.join(clone)
    }

    /// `b`'s changes written into the working tree at `dir`, as a result
    /// lands: the diff applied and the new files written.
    fn land(dir: &Path, b: &Bundle) {
        if !b.diff.is_empty() {
            apply_patch(dir, &b.diff).unwrap();
        }
        for f in &b.untracked {
            write_untracked(dir, f).unwrap();
        }
    }

    /// The job's state: an edit to kept.txt and a new file, uncommitted.
    fn job_state(dir: &Path) {
        std::fs::write(dir.join("kept.txt"), "base\njob\n").unwrap();
        std::fs::write(dir.join("job-new.txt"), "untracked").unwrap();
    }

    /// Slice 04 (R-ALLOC-4, D8): a job packed on a branch with two commits
    /// no remote has, an uncommitted change and an untracked file applies
    /// on a clone that never saw the commits, as an identical tree, with
    /// its state recorded as a snapshot; its prompt travels with it.
    #[test]
    fn alloc_04_a_job_with_unpushed_commits_applies_on_a_clone_that_never_saw_them() {
        let root = scratch("job");
        let a = origin_and_clone(&root, "a");
        git(&a, &["checkout", "-q", "-b", "local"]);
        for n in ["one", "two"] {
            std::fs::write(a.join(format!("{n}.txt")), n).unwrap();
            git(&a, &["add", "."]);
            git(&a, &["commit", "-q", "-m", n]);
        }
        job_state(&a);
        let head = git(&a, &["rev-parse", "HEAD"]);
        let job = pack_job(&a, "0190f3c2-0000-7000-8000-000000000001", "fix the test").unwrap();
        assert_eq!((job.base.as_str(), job.prompt.as_str()), (head.as_str(), "fix the test"));
        assert!(!job.commits.is_empty(), "the unpushed commits travel");
        let b = origin_and_clone(&root, "b");
        assert!(std::process::Command::new("git").args(["cat-file", "-e", &format!("{head}^{{commit}}")]).current_dir(&b).status().unwrap().code() != Some(0), "b has never seen it");
        let job = Bundle::decode(&job.encode(), "0190f3c2-0000-7000-8000-000000000001").unwrap();
        let landed = apply_job(&job, &b, &root.join("worktrees")).unwrap();
        let wt = &landed.applied.worktree.path;
        for (f, want) in [("one.txt", "one"), ("two.txt", "two"), ("kept.txt", "base\njob\n"), ("job-new.txt", "untracked")] {
            assert_eq!(std::fs::read_to_string(wt.join(f)).unwrap(), want, "{f}");
            assert_eq!(git(wt, &["show", &format!("{}:{f}", landed.snapshot)]), want.trim_end(), "the snapshot holds {f} as the job left it");
        }
        assert!(git(wt, &["merge-base", "--is-ancestor", &head, &landed.snapshot]).is_empty(), "on top of the job's base");
        assert_eq!(git(wt, &["log", "-1", "--format=%an <%ae>", &landed.snapshot]), "krowk <krowk@localhost>", "written as krowk, whatever the git config says");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 04 (R-ALLOC-4): a job leaves out what names what runs or what
    /// is allowed on the target, and the target refuses a job carrying it,
    /// or a base that is no commit id.
    #[test]
    fn alloc_04_a_job_never_carries_fenced_paths_and_a_target_refuses_one_that_does() {
        let root = scratch("job-fenced");
        let a = origin_and_clone(&root, "a");
        for dir in [".claude", ".codex", ".krowk"] {
            std::fs::create_dir_all(a.join(dir)).unwrap();
            std::fs::write(a.join(dir).join("settings.json"), "{}").unwrap();
        }
        std::fs::write(a.join("ok.txt"), "ok").unwrap();
        let job = pack_job(&a, "0190f3c2-0000-7000-8000-000000000001", "p").unwrap();
        assert_eq!(job.untracked.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["ok.txt"], "fenced files are left out");
        let b = origin_and_clone(&root, "b");
        let planted = Bundle { untracked: vec![Untracked { path: ".claude/settings.json".into(), executable: false, bytes: b"{}".to_vec() }], ..job.clone() };
        assert!(apply_job(&planted, &b, &root.join("worktrees")).is_err(), "a job carrying a fenced path is refused");
        let bad_base = Bundle { base: "--upload-pack=touch /tmp/x".into(), ..job.clone() };
        assert!(apply_job(&bad_base, &b, &root.join("worktrees")).is_err(), "a base that is no commit id is refused");

        // An unpushed commit that changes a fenced path: the supervisor
        // fails at once, and a job carrying one anyway is refused.
        git(&a, &["checkout", "-q", "-b", "local"]);
        git(&a, &["add", "-f", ".krowk/settings.json"]);
        git(&a, &["commit", "-q", "-m", "fenced"]);
        assert_eq!(pack_job(&a, "0190f3c2-0000-7000-8000-000000000001", "p").unwrap_err(), JOB_FENCED);
        git(&a, &["bundle", "create", "-q", "../planted.bundle", "local", "^origin/main"]);
        let planted = Bundle { base: git(&a, &["rev-parse", "HEAD"]), commits: std::fs::read(root.join("planted.bundle")).unwrap(), diff: Vec::new(), untracked: Vec::new(), ..job };
        assert!(apply_job(&planted, &b, &root.join("worktrees")).is_err(), "a job whose commits change a fenced path is refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-4: a base a remote has needs no git bundle: the target
    /// fetches it from origin, and a job at a clean tree has an empty diff.
    #[test]
    fn r_alloc_4_a_pushed_base_travels_without_commits_and_is_fetched_from_origin() {
        let root = scratch("job-pushed");
        let a = origin_and_clone(&root, "a");
        std::fs::write(a.join("pushed.txt"), "p").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-q", "-m", "pushed"]);
        git(&a, &["push", "-q", "origin", "HEAD"]);
        let job = pack_job(&a, "s", "go").unwrap();
        assert!(job.commits.is_empty() && job.diff.is_empty() && job.untracked.is_empty());
        let b = origin_and_clone(&root, "b");
        let landed = apply_job(&job, &b, &root.join("worktrees")).unwrap();
        assert_eq!(std::fs::read_to_string(landed.applied.worktree.path.join("pushed.txt")).unwrap(), "p");
        assert_eq!(landed.snapshot.len(), 40);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-4: a tracked file changed in a fenced directory, or a
    /// patch naming one, stays out of the job and is refused by the target.
    #[test]
    fn r_alloc_4_uncommitted_changes_in_a_fenced_directory_are_left_out() {
        let root = scratch("job-fenced-diff");
        let a = origin_and_clone(&root, "a");
        std::fs::create_dir_all(a.join(".claude")).unwrap();
        std::fs::write(a.join(".claude/settings.json"), "{}").unwrap();
        git(&a, &["add", "-f", "."]);
        git(&a, &["commit", "-q", "-m", "claude"]);
        git(&a, &["push", "-q", "origin", "HEAD"]);
        std::fs::write(a.join(".claude/settings.json"), "{\"x\":1}").unwrap();
        std::fs::write(a.join("kept.txt"), "base\nmore\n").unwrap();
        let job = pack_job(&a, "s", "go").unwrap();
        let patch = String::from_utf8(job.diff.clone()).unwrap();
        assert!(patch.contains("kept.txt") && !patch.contains(".claude"), "{patch}");
        let b = origin_and_clone(&root, "b");
        let planted = Bundle { diff: b"diff --git a/.claude/settings.json b/.claude/settings.json\n--- a/.claude/settings.json\n+++ b/.claude/settings.json\n@@ -1 +1 @@\n-{}\n+{\"x\":1}\n".to_vec(), ..job };
        assert!(apply_job(&planted, &b, &root.join("worktrees")).is_err());
        let rename = Bundle { diff: b"diff --git a/.claude/settings.json b/moved\nsimilarity index 100%\nrename from .claude/settings.json\nrename to moved\n".to_vec(), ..planted.clone() };
        assert!(apply_job(&rename, &b, &root.join("worktrees")).is_err(), "a rename out of a fenced directory");
        let tab = Bundle { diff: b"diff --git a/.claude/a\tb b/.claude/a\tb\nnew file mode 100644\n--- /dev/null\n+++ b/.claude/a\tb\n@@ -0,0 +1 @@\n+x\n".to_vec(), ..planted };
        assert!(apply_job(&tab, &b, &root.join("worktrees")).is_err(), "a tab in a fenced name");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 04 (R-ALLOC-4): past 64 MiB the job fails with the fix.
    #[test]
    fn alloc_04_a_job_over_64_mib_fails_with_the_fix() {
        let root = scratch("job-big");
        let a = origin_and_clone(&root, "a");
        std::fs::File::create(a.join("big.bin")).unwrap().set_len(MAX_JOB + 1).unwrap();
        assert_eq!(pack_job(&a, "0190f3c2-0000-7000-8000-000000000001", "p").unwrap_err(), JOB_TOO_BIG);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 06 (R-ALLOC-6, D10): a child's result is its work alone,
    /// against the job's snapshot, committed and uncommitted changes and
    /// new files alike, and lands on a working tree that holds the job's
    /// state; an unchanged child's result is empty.
    #[test]
    fn alloc_06_a_result_is_the_childs_work_against_the_jobs_snapshot() {
        let root = scratch("result");
        let child = origin_and_clone(&root, "child");
        job_state(&child);
        git(&child, &["add", "-A"]);
        git(&child, &["commit", "-q", "-m", "the job's state"]);
        let snapshot = git(&child, &["rev-parse", "HEAD"]);
        let unchanged = pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert!(unchanged.diff.is_empty() && unchanged.untracked.is_empty(), "an unchanged child's result is empty");
        std::fs::write(child.join("committed.txt"), "c").unwrap();
        git(&child, &["add", "."]);
        git(&child, &["commit", "-q", "-m", "child"]);
        std::fs::write(child.join("kept.txt"), "base\njob\nchild\n").unwrap();
        std::fs::write(child.join("new.txt"), "n").unwrap();
        let result = pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert_eq!(result.base, snapshot);
        assert!(result.untracked.iter().all(|f| f.path != "job-new.txt"), "the job's own files are not the child's work");
        let parent = origin_and_clone(&root, "parent");
        job_state(&parent);
        land(&parent, &result);
        for (f, want) in [("committed.txt", "c"), ("kept.txt", "base\njob\nchild\n"), ("new.txt", "n"), ("job-new.txt", "untracked")] {
            assert_eq!(std::fs::read_to_string(parent.join(f)).unwrap(), want, "{f}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-6: the child wrote the worktree's config and attributes, so
    /// packing its result runs none of its filters, and refuses a config
    /// that names another worktree or file.
    #[test]
    fn alloc_06_packing_a_result_runs_none_of_the_childs_filters() {
        let root = scratch("result-filter");
        let child = origin_and_clone(&root, "child");
        std::fs::write(child.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        git(&child, &["add", "-A"]);
        git(&child, &["commit", "-q", "-m", "the job's state"]);
        let snapshot = git(&child, &["rev-parse", "HEAD"]);
        let marker = root.join("marker");
        git(&child, &["config", "filter.evil.clean", &format!("touch {}; cat", marker.display())]);
        git(&child, &["config", "filter.evil.process", &format!("touch {}", marker.display())]);
        std::fs::write(child.join("new.txt"), "n").unwrap();
        std::fs::write(child.join("kept.txt"), "base\nedit\n").unwrap();
        let result = pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert!(!marker.exists(), "a filter ran");
        assert!(String::from_utf8_lossy(&result.diff).contains("new.txt"), "the work is still taken");
        git(&child, &["config", "core.worktree", "/tmp"]);
        assert_eq!(pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap_err(), RESULT_CONFIG);
        git(&child, &["config", "--unset", "core.worktree"]);
        git(&child, &["config", "include.path", "/tmp/x"]);
        assert_eq!(pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap_err(), RESULT_CONFIG);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-6: a repository inside the child's tree (a gitlink) is
    /// refused, and no index file is left behind.
    #[test]
    fn alloc_06_a_result_refuses_a_gitlink_and_leaves_no_index() {
        let root = scratch("result-link");
        let child = origin_and_clone(&root, "child");
        let snapshot = git(&child, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(child.join("inner")).unwrap();
        git(&child.join("inner"), &["init", "-q"]);
        git(&child.join("inner"), &["commit", "-q", "--allow-empty", "-m", "x"]);
        let err = pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap_err();
        assert!(err.contains("submodule"), "{err}");
        let left: Vec<_> = std::fs::read_dir(child.join(".git")).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("krowk-result-index")).collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-6: the size bound counts what the child changed, not what the
    /// repository holds: tracked files far past the cap with a tiny change
    /// pack, and an untracked file past it does not.
    #[test]
    fn alloc_06_the_size_bound_counts_the_childs_changes_only() {
        let root = scratch("result-cap");
        let child = origin_and_clone(&root, "child");
        std::fs::write(child.join("big.txt"), vec![b'x'; 4096]).unwrap();
        git(&child, &["add", "-A"]);
        git(&child, &["commit", "-q", "-m", "the job's state"]);
        let snapshot = git(&child, &["rev-parse", "HEAD"]);
        std::fs::write(child.join("small.txt"), "s").unwrap();
        let ok = pack_result_within(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002", 1000).unwrap();
        assert!(String::from_utf8_lossy(&ok.diff).contains("small.txt"));
        std::fs::write(child.join("huge.bin"), vec![b'y'; 2048]).unwrap();
        assert_eq!(pack_result_within(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002", 1000).unwrap_err(), RESULT_TOO_BIG);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-6: a linked worktree, where a task's child runs, packs.
    #[test]
    fn alloc_06_a_linked_worktree_packs() {
        let root = scratch("result-linked");
        let main = origin_and_clone(&root, "main");
        let snapshot = git(&main, &["rev-parse", "HEAD"]);
        let wt = root.join("wt");
        git(&main, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), &snapshot]);
        std::fs::write(wt.join("new.txt"), "n").unwrap();
        let r = pack_result(&wt, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert!(String::from_utf8_lossy(&r.diff).contains("new.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-ALLOC-6: a `.git` that is a link is none the child's own worktree.
    #[cfg(unix)]
    #[test]
    fn alloc_06_a_worktree_whose_git_dir_is_a_link_is_refused() {
        let root = scratch("result-dotgit");
        let child = origin_and_clone(&root, "child");
        let snapshot = git(&child, &["rev-parse", "HEAD"]);
        std::fs::rename(child.join(".git"), root.join("elsewhere.git")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere.git"), child.join(".git")).unwrap();
        assert_eq!(pack_result(&child, &snapshot, "0190f3c2-0000-7000-8000-000000000002").unwrap_err(), RESULT_CONFIG);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 07 (R-ALLOC-8, D10): a result lands in the parent's working
    /// tree, which holds the job's state, never its index or HEAD; one that
    /// conflicts is kept in a local worktree and the files named.
    #[test]
    fn alloc_07_a_result_lands_in_the_working_tree_or_is_kept_on_conflict() {
        use crate::worktree::apply::Applied as Landed;
        let root = scratch("land");
        let parent = origin_and_clone(&root, "parent");
        let base = git(&parent, &["rev-parse", "HEAD"]);
        job_state(&parent);
        let job = pack(&parent, "s", 0, None).unwrap();
        // The child: the job's state as its snapshot, then its own work.
        let child = origin_and_clone(&root, "child");
        land(&child, &job);
        git(&child, &["add", "-A"]);
        git(&child, &["commit", "-q", "-m", "snapshot"]);
        std::fs::write(child.join("kept.txt"), "base\njob\nchild\n").unwrap();
        std::fs::write(child.join("new.txt"), "n").unwrap();
        let result = pack(&child, "s", 0, None).unwrap();

        let (_, landed) = apply_result(&job, &result, &parent, &root.join("worktrees")).unwrap();
        assert!(matches!(&landed, Landed::Applied(files) if files.len() == 2 && files.contains(&"kept.txt".to_string()) && files.contains(&"new.txt".to_string())), "the child's work alone: {landed:?}");
        assert_eq!(std::fs::read_to_string(parent.join("kept.txt")).unwrap(), "base\njob\nchild\n");
        assert_eq!(git(&parent, &["rev-parse", "HEAD"]), base, "HEAD untouched");
        assert_eq!(git(&parent, &["diff", "--cached", "--name-only"]), "", "index untouched");
        let (kept, landed) = apply_result(&job, &result, &parent, &root.join("worktrees")).unwrap();
        assert!(matches!(&landed, Landed::Conflicts(files) if !files.is_empty()), "{landed:?}");
        assert!(kept.path.exists(), "a result that conflicts is kept in a worktree");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R-HAND-5: a backend session's transcript travels in the bundle and
    /// lands where Claude Code looks for it from the new working directory,
    /// so `claude --resume <id>` there finds it.
    #[test]
    fn r_hand_5_the_claude_transcript_moves_to_where_resume_finds_it() {
        let root = std::env::temp_dir().join(format!("krowk-handoff-r5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (repo, src_home, dst_home) = (root.join("repo"), root.join("a-claude"), root.join("b-claude"));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).current_dir(&repo).status().unwrap().success());
        git(&["init", "-q"]);
        std::fs::write(repo.join("f"), "x").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-q", "-m", "base"]);
        let id = "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0";
        let at = crate::claude::transcript_path(&src_home, &repo.display().to_string(), id);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, "{\"type\":\"user\"}\n").unwrap();
        let b = pack(&repo, "s", i64::MAX, Some((crate::claude::BACKEND, id, &at))).unwrap();
        assert!(b.untracked.is_empty(), "nothing changed since the session started");
        let new_cwd = root.join("worktree/sub");
        let placed = place_transcript(&b, &dst_home, &new_cwd).unwrap().expect("carried");
        assert_eq!(placed, crate::claude::transcript_path(&dst_home, &new_cwd.display().to_string(), id));
        assert_eq!(std::fs::read(&placed).unwrap(), std::fs::read(&at).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
