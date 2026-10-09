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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// The bundle's format.
pub const BUNDLE_V1: u32 = 1;

/// The most the untracked files together may be: past it the handoff is
/// refused, rather than shipping build output nobody meant to move.
pub const MAX_UNTRACKED: u64 = 64 << 20;

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
    Ok(Bundle { v: BUNDLE_V1, session: session.to_string(), base, subdir, diff, untracked, transcript })
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
    let mut seen = std::collections::HashSet::new();
    let events: Vec<&Value> = events.iter().filter(|e| e["id"].as_str().is_some_and(|i| seen.insert(i.to_string()))).collect();
    let dir = sessions.join(id);
    let file = dir.join(EVENTS_FILE);
    if let Ok(here) = std::fs::read_to_string(&file) {
        let ours = here.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|e| e["id"].as_str().map(String::from)).find(|i| !seen.contains(i));
        if let Some(extra) = ours {
            return Err(format!("this machine's log of session {id} holds event {extra}, which the synced log does not — it went on here after it left; nothing was replaced"));
        }
    }
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
        let b = Bundle { v: BUNDLE_V1, session: "s".into(), base: "abc".into(), subdir: "".into(), diff: vec![0, 1, 255], untracked: vec![Untracked { path: "a".into(), executable: true, bytes: b"x".to_vec() }], transcript: None };
        assert_eq!(Bundle::decode(&b.encode(), "s").unwrap(), b);
        assert!(Bundle::decode(&b.encode(), "t").is_err());
        let newer = Bundle { v: BUNDLE_V1 + 1, ..b };
        assert!(Bundle::decode(&newer.encode(), "s").unwrap_err().contains("upgrade"));
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
