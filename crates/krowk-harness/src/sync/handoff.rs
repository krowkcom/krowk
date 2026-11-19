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
//! bundle of the supervisor's directory (all its untracked files, not only
//! those changed since a session started) plus `commits`, a git bundle of
//! the commits on the way to `base` that no remote has, so a branch never
//! pushed still arrives: `pack_job`, at most `MAX_JOB` in all, and
//! `apply_job` on the target. A result is the bundle of the child's
//! worktree against the job's base, everything it changed, committed or
//! not, as `diff` and `untracked`: `pack_result` on the target when the
//! child's one turn completes, named in the child's log (`ResultNote`), and
//! `apply_result` on the supervisor's machine, which lands it as a local
//! worktree subagent's changes land (`crate::worktree::apply`). Both
//! travel as transport artifacts like a handoff's.

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
}

/// What the child's log records when its one turn completes (R-ALLOC-6,
/// R-ALLOC-14), as `RESULT_EVENT`: the result's transport and key, none
/// when the child changed nothing, and the approvals it was denied because
/// a remote child cannot ask. The summary is the turn's final message and
/// its usage the turn's `turn.completed`, already in the log. Slice 06
/// writes it as a log event of this shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultNote {
    pub turn_id: String,
    /// The result's transport slug and key (64 hex digits), stored under
    /// the child's session; none for an unchanged child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The job's base, which the result is against.
    pub base: String,
    /// Each denied approval in a line: the tool and what it asked, or the
    /// ask rule that became a denial.
    #[serde(default)]
    pub denied: Vec<String>,
}

/// The log event type a `ResultNote` is recorded under.
pub const RESULT_EVENT: &str = "task.result";

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
    let out = krowk_api::git::query(dir).and_then(|mut c| c.args(args).output()).map_err(|e| format!("git {}: {e}", args[0]))?;
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

/// The source's half: the bundle of the session running in `cwd`, started
/// at `since_ms`. `transcript` is the vendor's file the log names, read
/// only when it is one: a regular file named for its session, not a link.
pub fn pack(cwd: &Path, session: &str, since_ms: i64, transcript: Option<(&str, &str, &Path)>) -> Result<Bundle, String> {
    let top = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| format!("{} is not in a git repository, so its work cannot move ({e})", cwd.display()))?);
    let base = git_text(&top, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(|_| "the repository has no commit yet to move from".to_string())?;
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let subdir = canon(cwd).strip_prefix(canon(&top)).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let diff = git_out(&top, &["diff", "--binary", "--no-ext-diff", "--no-textconv", "--no-renames", "HEAD", "--"])?;
    let listed = git_out(&top, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked = Vec::new();
    let mut total = 0u64;
    for rel in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let rel = String::from_utf8(rel.to_vec()).map_err(|_| "an untracked file's name is not UTF-8, so it cannot move".to_string())?;
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
        if total > MAX_UNTRACKED {
            return Err(format!("the untracked files the session changed come to more than {} MB — commit or ignore what should not move (it was {rel} that went over)", MAX_UNTRACKED >> 20));
        }
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        untracked.push(Untracked { path: rel, executable, bytes });
    }
    let transcript = match transcript {
        Some((backend, id, path)) if crate::handoff::valid_session_id(id) && path.file_name().and_then(|n| n.to_str()) == Some(&format!("{id}.jsonl")) && path.symlink_metadata().is_ok_and(|m| m.is_file()) => {
            Some(Transcript { backend: backend.to_string(), vendor_session_id: id.to_string(), bytes: std::fs::read(path).map_err(|e| format!("read the transcript {}: {e}", path.display()))? })
        }
        _ => None,
    };
    Ok(Bundle { v: BUNDLE_V1, session: session.to_string(), base, subdir, diff, untracked, transcript, commits: Vec::new() })
}

/// The supervisor's half of a job (R-ALLOC-4): the bundle of `cwd`'s
/// repository at HEAD, its uncommitted changes and untracked files, and the
/// commits no remote has, for the transport stored under `session`; fails
/// with `JOB_TOO_BIG` past `MAX_JOB`. Slice 04 builds it.
pub fn pack_job(cwd: &Path, session: &str) -> Result<Bundle, String> {
    todo!("slice 04: pack the job of {} for {session}", cwd.display())
}

/// The target's half of a job: as `apply`, a fresh worktree of `repo` at
/// the job's base with its changes, `base` fetched from `commits` first
/// when the clone lacks it (then from origin). Slice 04 builds it.
pub fn apply_job(b: &Bundle, repo: &Path, worktrees: &Path) -> Result<Applied, String> {
    todo!("slice 04: apply job {} in {} under {}", b.base, repo.display(), worktrees.display())
}

/// The child's result (R-ALLOC-6): everything the worktree at `worktree`
/// changed against `base`, committed or not, untracked files included, for
/// the transport stored under the child's `session`. Slice 06 builds it.
pub fn pack_result(worktree: &Path, base: &str, session: &str) -> Result<Bundle, String> {
    todo!("slice 06: pack the result of {} against {base} for {session}", worktree.display())
}

/// The supervisor lands a result (R-ALLOC-8, D10): it is applied into a
/// fresh worktree of `to`'s repository at its base, under `worktrees`, and
/// that worktree's changes to `to`'s working tree as a local subagent's are
/// (`Approval::Agent`: never index or HEAD, whole or not at all, kept and
/// named on conflict, fenced paths and submodules kept). The worktree, and
/// what came of it. Slice 07 builds it.
pub fn apply_result(b: &Bundle, to: &Path, worktrees: &Path) -> Result<(crate::worktree::Worktree, crate::worktree::apply::Applied), String> {
    todo!("slice 07: apply the result at {} to {} under {}", b.base, to.display(), worktrees.display())
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
        let _ = krowk_api::git::command(repo).and_then(|mut c| c.env("GIT_TERMINAL_PROMPT", "0").args(["fetch", "--quiet", "origin"]).output());
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

/// `git apply --binary` of `patch` in `dir`, fed on stdin.
fn apply_patch(dir: &Path, patch: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut c = krowk_api::git::command(dir).map_err(|e| e.to_string())?;
    let mut child = c.args(["apply", "--binary", "--whitespace=nowarn", "-"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("piped");
    let feed = std::thread::scope(|s| {
        let w = s.spawn(move || stdin.write_all(patch));
        let out = child.wait_with_output();
        (w.join().unwrap_or(Ok(())), out)
    });
    let out = feed.1.map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
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
pub fn fetch_images(client: &krowk_api::Client, keys: &krowk_client::e2e::SessionKeys, id: &str, session_dir: &Path, events: &[Value]) -> Result<(), String> {
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
    krowk_client::e2e::hex(key)
}

pub fn key_from_hex(hex: &str) -> Option<[u8; 32]> {
    krowk_client::e2e::unhex(hex).and_then(|v| v.try_into().ok())
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
        let b = Bundle { v: BUNDLE_V1, session: "s".into(), base: "abc".into(), subdir: "".into(), diff: vec![0, 1, 255], untracked: vec![Untracked { path: "a".into(), executable: true, bytes: b"x".to_vec() }], transcript: None, commits: Vec::new() };
        assert_eq!(Bundle::decode(&b.encode(), "s").unwrap(), b);
        assert!(Bundle::decode(&b.encode(), "t").is_err());
        assert!(!String::from_utf8(b.encode()).unwrap().contains("commits"), "a handoff's bundle is written as before");
        let newer = Bundle { v: BUNDLE_V1 + 1, ..b };
        assert!(Bundle::decode(&newer.encode(), "s").unwrap_err().contains("upgrade"));
    }

    /// R-ALLOC-4: a job is a bundle with its unpushed commits, and reads
    /// back as it went in; a bundle written before jobs reads as one with
    /// none.
    #[test]
    fn r_alloc_4_a_job_round_trips_with_its_commits() {
        let job = Bundle { v: BUNDLE_V1, session: "s".into(), base: "abc".into(), subdir: "sub".into(), diff: Vec::new(), untracked: Vec::new(), transcript: None, commits: b"# v2 git bundle\n\0\xff".to_vec() };
        assert_eq!(Bundle::decode(&job.encode(), "s").unwrap(), job);
        let old = br#"{"v":1,"session":"s","base":"abc","subdir":"","diff":"","untracked":[]}"#;
        assert!(Bundle::decode(old, "s").unwrap().commits.is_empty());
    }

    /// R-ALLOC-6: the note a child's log names its result with reads back
    /// as it went in, and an unchanged child's names no transport.
    #[test]
    fn r_alloc_6_a_result_note_round_trips() {
        let note = ResultNote { turn_id: "t".into(), transport: Some("art_1".into()), key: Some("ab".repeat(32)), base: "abc".into(), denied: vec!["bash: rm -rf build".into()] };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["turnId"], "t");
        assert_eq!(serde_json::from_value::<ResultNote>(json).unwrap(), note);
        let unchanged: ResultNote = serde_json::from_str(r#"{"turnId":"t","base":"abc"}"#).unwrap();
        assert_eq!((unchanged.transport, unchanged.denied.len()), (None, 0));
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
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q"]);
        std::fs::write(seed.join("kept.txt"), "base\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        git(root, &["clone", "-q", "--bare", "seed", "origin.git"]);
        git(root, &["clone", "-q", "origin.git", clone]);
        root.join(clone)
    }

    /// Slice 04 (R-ALLOC-4, D8): a job packed on a branch with two commits
    /// no remote has, an uncommitted change and an untracked file applies
    /// on a clone that never saw the commits, as an identical tree.
    #[test]
    #[ignore = "slice 04: pack_job and apply_job"]
    fn alloc_04_a_job_with_unpushed_commits_applies_on_a_clone_that_never_saw_them() {
        let root = scratch("job");
        let a = origin_and_clone(&root, "a");
        git(&a, &["checkout", "-q", "-b", "local"]);
        for n in ["one", "two"] {
            std::fs::write(a.join(format!("{n}.txt")), n).unwrap();
            git(&a, &["add", "."]);
            git(&a, &["commit", "-q", "-m", n]);
        }
        std::fs::write(a.join("kept.txt"), "base\nedited\n").unwrap();
        std::fs::write(a.join("new.txt"), "untracked").unwrap();
        let head = git(&a, &["rev-parse", "HEAD"]);
        let job = pack_job(&a, "0190f3c2-0000-7000-8000-000000000001").unwrap();
        assert_eq!(job.base, head);
        assert!(!job.commits.is_empty(), "the unpushed commits travel");
        let b = root.join("b");
        git(&root, &["clone", "-q", "origin.git", "b"]);
        assert!(std::process::Command::new("git").args(["cat-file", "-e", &format!("{head}^{{commit}}")]).current_dir(&b).status().unwrap().code() != Some(0), "b has never seen it");
        let job = Bundle::decode(&job.encode(), "0190f3c2-0000-7000-8000-000000000001").unwrap();
        let landed = apply_job(&job, &b, &root.join("worktrees")).unwrap();
        let wt = &landed.worktree.path;
        assert_eq!(git(wt, &["rev-parse", "HEAD"]), head);
        for (f, want) in [("one.txt", "one"), ("two.txt", "two"), ("kept.txt", "base\nedited\n"), ("new.txt", "untracked")] {
            assert_eq!(std::fs::read_to_string(wt.join(f)).unwrap(), want, "{f}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 04 (R-ALLOC-4): past 64 MiB the job fails with the fix.
    #[test]
    #[ignore = "slice 04: pack_job's bound"]
    fn alloc_04_a_job_over_64_mib_fails_with_the_fix() {
        let root = scratch("job-big");
        let a = origin_and_clone(&root, "a");
        std::fs::File::create(a.join("big.bin")).unwrap().set_len(MAX_JOB + 1).unwrap();
        assert_eq!(pack_job(&a, "0190f3c2-0000-7000-8000-000000000001").unwrap_err(), JOB_TOO_BIG);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 06 (R-ALLOC-6, D10): a child's result, committed and
    /// uncommitted changes and new files alike, reproduces its tree on the
    /// base; an unchanged child's result is empty.
    #[test]
    #[ignore = "slice 06: pack_result"]
    fn alloc_06_a_result_reproduces_the_childs_changes_on_the_base() {
        let root = scratch("result");
        let child = origin_and_clone(&root, "child");
        let base = git(&child, &["rev-parse", "HEAD"]);
        let unchanged = pack_result(&child, &base, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert!(unchanged.diff.is_empty() && unchanged.untracked.is_empty(), "an unchanged child's result is empty");
        std::fs::write(child.join("committed.txt"), "c").unwrap();
        git(&child, &["add", "."]);
        git(&child, &["commit", "-q", "-m", "child"]);
        std::fs::write(child.join("kept.txt"), "base\nchild\n").unwrap();
        std::fs::write(child.join("new.txt"), "n").unwrap();
        let result = pack_result(&child, &base, "0190f3c2-0000-7000-8000-000000000002").unwrap();
        assert_eq!(result.base, base);
        let other = root.join("other");
        git(&root, &["clone", "-q", "origin.git", "other"]);
        let landed = apply(&result, &other, &root.join("worktrees")).unwrap();
        for (f, want) in [("committed.txt", "c"), ("kept.txt", "base\nchild\n"), ("new.txt", "n")] {
            assert_eq!(std::fs::read_to_string(landed.worktree.path.join(f)).unwrap(), want, "{f}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Slice 07 (R-ALLOC-8, D10): a result lands in the parent's working
    /// tree, never its index or HEAD; one that conflicts is kept in a local
    /// worktree and the files named.
    #[test]
    #[ignore = "slice 07: apply_result"]
    fn alloc_07_a_result_lands_in_the_working_tree_or_is_kept_on_conflict() {
        use crate::worktree::apply::Applied as Landed;
        let root = scratch("land");
        let parent = origin_and_clone(&root, "parent");
        let base = git(&parent, &["rev-parse", "HEAD"]);
        let patch = b"diff --git a/kept.txt b/kept.txt\n--- a/kept.txt\n+++ b/kept.txt\n@@ -1 +1,2 @@\n base\n+child\n".to_vec();
        let result = Bundle { v: BUNDLE_V1, session: "s".into(), base: base.clone(), subdir: String::new(), diff: patch, untracked: vec![Untracked { path: "new.txt".into(), executable: false, bytes: b"n".to_vec() }], transcript: None, commits: Vec::new() };
        let (_, landed) = apply_result(&result, &parent, &root.join("worktrees")).unwrap();
        assert!(matches!(&landed, Landed::Applied(files) if files.contains(&"kept.txt".to_string()) && files.contains(&"new.txt".to_string())), "{landed:?}");
        assert_eq!(std::fs::read_to_string(parent.join("kept.txt")).unwrap(), "base\nchild\n");
        assert_eq!(git(&parent, &["rev-parse", "HEAD"]), base, "HEAD untouched");
        assert_eq!(git(&parent, &["diff", "--cached", "--name-only"]), "", "index untouched");
        let (kept, landed) = apply_result(&result, &parent, &root.join("worktrees")).unwrap();
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
