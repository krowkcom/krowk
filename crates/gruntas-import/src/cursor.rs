//! Cursor's agent transcripts: `~/.cursor/projects/<slug>/agent-transcripts/<id>/<id>.jsonl`,
//! with a `repo.json` beside agent-transcripts holding a repo id, not a path.
//!
//! Nothing in a transcript names a message — no uuid per line, no id on a
//! tool_use — so every message has an empty foreign id and the store appends
//! it unconditionally. That is why this source, unlike the others, honors the
//! cursor for messages: re-reading the whole file would duplicate every row.
//! An id-less tool_use is keyed by its absolute line, `cursor:<line>`, and
//! later ones on the same line by `cursor:<line>:<k>`, so a delta read never
//! reuses an id an earlier read emitted. Turns and events are positional
//! cumulative lists in the store, so turns are always computed over the whole
//! file and the repo.json event rides full reads only.
//!
//! The transcript names no time, directory or model. The slug decodes to a
//! path by turning `-` into `/`, which is not invertible, so the path is used
//! only when that directory exists; otherwise the worktree is `cursor:<slug>`.
//!
//! No tool_result was ever observed, so that shape is held leniently: several
//! id and payload spellings, and a result that pairs with no call in this read
//! is kept and counted as `tool_result:unlinked`.

use crate::{
    Env, ImportError, JsonlCursor, LineError, PART_TEXT, PART_TOOL_RESULT, ReadResult, Ref, Source, TurnCandidate,
    decode_jsonl_cursor, encode_cursor, home_path, jsonl_unchanged, new_tool_call_part, new_tool_result_part, open_home,
    read_home, read_jsonl, split_turns,
};
use gruntas_store::{Binding, Event, Message, Part, Role, Session, Thread, Turn, Worktree};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

pub const PROVIDER: &str = "cursor";
pub const HARNESS: &str = "cursor";

const PROJECTS_DIR: &str = ".cursor/projects";
const TRANSCRIPTS_DIR: &str = "agent-transcripts";
const EVENT_REPO: &str = "cursor_repo";
const VCS_GIT: &str = "git";
const VCS_NONE: &str = "none";

pub struct Cursor;

impl Source for Cursor {
    fn name(&self) -> &'static str {
        crate::PROVIDER_CURSOR
    }

    /// Every session transcript, sorted by slug then id so two runs agree. A
    /// machine with no Cursor has none; an unreadable slug is skipped.
    fn discover(&self, env: Env) -> Result<Vec<Ref>, ImportError> {
        crate::check_os().map_err(|e| prefixed(e, "cursor"))?;
        let root = match home_path(env, PROJECTS_DIR) {
            Ok(root) => root,
            // Go's errors.Is(fs.ErrNotExist): a home that is not there is a
            // machine with nothing on it, not a failure.
            Err(ImportError::Other(_)) if !Path::new(&crate::home_dir(env)).exists() => return Ok(Vec::new()),
            Err(e) => return Err(prefixed(e, "cursor: resolve projects directory")),
        };
        let slugs = match sorted_dir(&root) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ImportError::Other(format!("cursor: list projects: {e}"))),
        };
        let mut refs = Vec::new();
        for (slug, is_dir) in slugs {
            if !is_dir {
                continue;
            }
            let Ok(sessions) = sorted_dir(&root.join(&slug).join(TRANSCRIPTS_DIR)) else { continue };
            for (id, is_dir) in sessions {
                // Discover names only what Read can open: the leaf must be a
                // regular file, since Read refuses a symlinked one.
                let name = format!("{id}.jsonl");
                let leaf = root.join(&slug).join(TRANSCRIPTS_DIR).join(&id).join(&name);
                if !is_dir || !std::fs::symlink_metadata(&leaf).is_ok_and(|m| m.file_type().is_file()) {
                    continue;
                }
                refs.push(Ref {
                    provider: self.name().into(),
                    path: format!("{PROJECTS_DIR}/{slug}/{TRANSCRIPTS_DIR}/{id}/{name}"),
                    id,
                });
            }
        }
        Ok(refs)
    }

    /// Messages from `cursor` on, turns over the whole file. A resumed read
    /// runs a second pass from zero for turn candidates only — its messages
    /// and counts are discarded — because the store's turn list is cumulative
    /// and a delta of it would insert nothing.
    fn read(&self, env: Env, r: &Ref, cursor: &str) -> Result<(Thread, String, ReadResult), ImportError> {
        let held = decode_jsonl_cursor(cursor).map_err(|e| prefixed(e, "cursor"))?;
        let (mut file, _) = open_home(env, &r.path, 0).map_err(|e| prefixed(e, &format!("cursor: open {}", r.path)))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let read_err = |e| prefixed(e, &format!("cursor: read {}", r.path));

        let mut b = Builder::new(r, held.offset == 0, line_base(&mut file, held, size));
        let (next, res) = read_jsonl(&mut file, held, |n, raw| b.line(n, raw)).map_err(read_err)?;
        // A file shorter than the cursor recorded was rescanned from zero:
        // that is a full read, so the repo sidecar is re-emitted.
        if held.offset != 0 && next.size < held.size {
            b.full = true;
        }
        b.acc.merge(&res);
        if b.full {
            b.repo_event(env);
        }
        let mut th = b.thread();
        if held.offset != 0 {
            let mut fb = Builder::new(r, false, 0);
            read_jsonl(&mut file, JsonlCursor::default(), |n, raw| fb.line(n, raw)).map_err(read_err)?;
            if !fb.candidates.is_empty() {
                th.turns = turns_from(&fb.candidates);
            }
        }
        Ok((th, encode_cursor(&next), b.acc))
    }

    fn unchanged(&self, env: Env, r: &Ref, cursor: &str) -> bool {
        jsonl_unchanged(env, r, cursor)
    }
}

/// A directory's entries as (name, is_dir), sorted by name. is_dir does not
/// follow symlinks, as Go's DirEntry.IsDir.
fn sorted_dir(dir: &Path) -> std::io::Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        out.push((e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())));
    }
    out.sort();
    Ok(out)
}

/// The error with its message prefixed, kind kept so the CLI can still tell a
/// refusal from an I/O failure.
fn prefixed(e: ImportError, prefix: &str) -> ImportError {
    use ImportError::*;
    let p = |m: String| format!("{prefix}: {m}");
    match e {
        NoHome(m) => NoHome(p(m)),
        OutsideHome(m) => OutsideHome(p(m)),
        EscapingSymlink(m) => EscapingSymlink(p(m)),
        NotRegularFile(m) => NotRegularFile(p(m)),
        TooLarge(m) => TooLarge(p(m)),
        UnsupportedOs(m) => UnsupportedOs(p(m)),
        Other(m) => Other(p(m)),
    }
}

/// One transcript as it is walked.
struct Builder<'a> {
    r: &'a Ref,
    acc: ReadResult,
    /// Whether this read started at the top; only a full read emits the
    /// repo event, or every delta would append a duplicate.
    full: bool,
    /// Newlines before the resume offset, so per-read line N is file line
    /// base+N wherever the read started.
    base: u64,
    /// Every tool_call_id this read emitted; pairing is per read.
    calls: HashSet<String>,
    messages: Vec<Message>,
    candidates: Vec<TurnCandidate>,
    events: Vec<Event>,
    /// The line the last synthesised tool id keyed, and how many followed it.
    tool_abs: u64,
    tool_n: usize,
}

impl<'a> Builder<'a> {
    fn new(r: &'a Ref, full: bool, base: u64) -> Builder<'a> {
        Builder {
            r,
            acc: ReadResult::default(),
            full,
            base,
            calls: HashSet::new(),
            messages: Vec::new(),
            candidates: Vec::new(),
            events: Vec::new(),
            tool_abs: 0,
            tool_n: 0,
        }
    }

    /// A role line becomes a message; a typed line with no envelope is
    /// furniture counted under its type, whatever role it claims; anything
    /// else is skipped.
    fn line(&mut self, line_no: usize, raw: &[u8]) -> Result<(), LineError> {
        let bad = |why: &str| LineError::Skip(format!("cursor: unreadable line: {why}"));
        let v: Value = crate::decode_line(raw).map_err(|e| bad(&e.to_string()))?;
        // Go decodes a literal null into the zero struct.
        let empty = Map::new();
        let obj = match &v {
            Value::Object(o) => o,
            Value::Null => &empty,
            _ => return Err(bad("line is not an object")),
        };
        let role = str_field(obj, "role").map_err(|_| bad("role is not a string"))?;
        let kind = str_field(obj, "type").map_err(|_| bad("type is not a string"))?;
        let message = match obj.get("message") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some(m),
            Some(_) => return Err(bad("message is not an object")),
        };
        if !kind.is_empty() && message.is_none() {
            self.acc.classify(&kind);
            return Ok(());
        }
        let role = match role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            "tool" => Role::Tool,
            "error" => Role::Error,
            _ if !kind.is_empty() => {
                self.acc.classify(&kind);
                return Ok(());
            }
            _ => return Err(LineError::Skip("cursor: line has no role".into())),
        };
        let parts = message.map(|m| self.parts(m.get("content"), line_no)).unwrap_or_default();
        self.candidates.push(TurnCandidate { role: Some(role), part_types: parts.iter().map(|p| p.kind.clone()).collect(), ..Default::default() });
        self.messages.push(Message {
            role,
            provider: PROVIDER.into(),
            model: String::new(),
            foreign_id: String::new(),
            usage: String::new(),
            raw_json: std::str::from_utf8(raw).ok().map(String::from),
            turn_seq: None,
            parts,
        });
        Ok(())
    }

    /// Content is an array of blocks on every transcript seen; a bare string
    /// reads the same as one text block, and any other shape is one counted
    /// unknown part rather than a silently empty message.
    fn parts(&mut self, content: Option<&Value>, line_no: usize) -> Vec<Part> {
        match content {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => text_part(s).into_iter().collect(),
            Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| self.block(b, line_no)).collect(),
            Some(other) => vec![self.acc.normalize_part("message_content", Some(other))],
        }
    }

    /// One content block; None for empty text, which a bare "" also yields.
    fn block(&mut self, raw: &Value, line_no: usize) -> Option<Part> {
        let blk = match raw {
            // A null element is a hole, counted, not empty text.
            Value::Null => return Some(self.acc.normalize_part("block", Some(raw))),
            Value::String(s) => return text_part(s),
            Value::Object(o) => o,
            _ => return Some(self.acc.normalize_part("block", Some(raw))),
        };
        // Go decodes the block into a typed struct: a field of the wrong type
        // fails the whole block, which then lands as an unknown "block".
        let Some(b) = ContentBlock::from(blk) else { return Some(self.acc.normalize_part("block", Some(raw))) };
        match b.kind.as_str() {
            "text" => match blk.get("text") {
                Some(Value::String(s)) => text_part(s),
                None | Some(Value::Null) => None,
                // A non-string text keeps its verbatim block under the text type.
                Some(_) => Some(self.acc.normalize_part(PART_TEXT, Some(raw))),
            },
            "tool_use" => {
                let mut id = b.id;
                if id.is_empty() {
                    let abs = self.base + line_no as u64;
                    if abs == self.tool_abs {
                        self.tool_n += 1;
                        id = format!("cursor:{abs}:{}", self.tool_n);
                    } else {
                        (self.tool_abs, self.tool_n) = (abs, 0);
                        id = format!("cursor:{abs}");
                    }
                }
                self.calls.insert(id.clone());
                Some(new_tool_call_part(&id, &b.name, blk.get("input")))
            }
            "tool_result" => {
                let id = [b.tool_use_id, b.call_id_camel, b.call_id_snake, b.id].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
                let output = ["content", "output"]
                    .into_iter()
                    .filter_map(|k| blk.get(k))
                    .find(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(Value::Null);
                Some(new_tool_result_part(&id, Some(&output), b.is_error_camel || b.is_error_snake))
            }
            other => Some(self.acc.normalize_part(other, Some(raw))),
        }
    }

    /// The repo.json sidecar, `<slug>/repo.json` beside agent-transcripts, as
    /// one event. Missing, unreadable or id-less is no event: the sidecar is a
    /// note, the transcript is the session.
    fn repo_event(&mut self, env: Env) {
        // A ref not shaped like a transcript has no sidecar; without this the
        // join would read $HOME/repo.json.
        if slug_of(&self.r.path).is_empty() {
            return;
        }
        let up = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
        let rel = up(&up(&up(Path::new(&self.r.path)))).join("repo.json");
        let Ok(data) = read_home(env, &rel.to_string_lossy(), 0) else { return };
        let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&data) else { return };
        let mut fields = Vec::new();
        for k in ["repo_id", "repoId", "id"] {
            match str_field(&o, k) {
                Ok(s) => fields.push(s),
                Err(()) => return,
            }
        }
        if let Some(id) = fields.into_iter().find(|s| !s.is_empty()) {
            self.events.push(Event { kind: EVENT_REPO.into(), data: go_json(&json!({ "repo_id": id })) });
        }
    }

    /// The accumulated thread. Linkage is reconciled here, once every call in
    /// the read has been seen, so a result before its call still links.
    fn thread(&mut self) -> Thread {
        let slug = slug_of(&self.r.path);
        let (mut path, mut vcs) = worktree_of(slug);
        if path.is_empty() {
            // No directory on disk: file under the slug, or the session id
            // when the ref has no slug at all.
            path = format!("cursor:{}", if slug.is_empty() { &self.r.id } else { slug });
            vcs = VCS_NONE;
            self.acc.classify("worktree-fallback");
        }
        let unlinked = self
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| p.kind == PART_TOOL_RESULT && (p.tool_call_id.is_empty() || !self.calls.contains(&p.tool_call_id)))
            .count();
        for _ in 0..unlinked {
            self.acc.classify("tool_result:unlinked");
        }
        Thread {
            worktree: Worktree { name: base_name(&path), path, vcs: vcs.into() },
            session: Session { provider: PROVIDER.into(), harness: HARNESS.into(), ..Session::default() },
            binding: Binding {
                provider: crate::PROVIDER_CURSOR.into(),
                harness: HARNESS.into(),
                foreign_session_id: self.r.id.clone(),
                // Resuming is unknown in v1.
                resume_cmd: String::new(),
            },
            parent: None,
            turns: turns_from(&self.candidates),
            events: std::mem::take(&mut self.events),
            messages: std::mem::take(&mut self.messages),
        }
    }
}

/// The fields of a content block Go decodes into a typed struct; None when
/// one has the wrong JSON type.
struct ContentBlock {
    kind: String,
    id: String,
    name: String,
    tool_use_id: String,
    call_id_camel: String,
    call_id_snake: String,
    is_error_camel: bool,
    is_error_snake: bool,
}

impl ContentBlock {
    fn from(o: &Map<String, Value>) -> Option<ContentBlock> {
        let s = |k| str_field(o, k).ok();
        let b = |k| match o.get(k) {
            None | Some(Value::Null) => Some(false),
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => None,
        };
        Some(ContentBlock {
            kind: s("type")?,
            id: s("id")?,
            name: s("name")?,
            tool_use_id: s("tool_use_id")?,
            call_id_camel: s("callID")?,
            call_id_snake: s("call_id")?,
            is_error_camel: b("isError")?,
            is_error_snake: b("is_error")?,
        })
    }
}

/// A string field as Go decodes one: absent or null is "", anything but a
/// string is an error.
fn str_field(o: &Map<String, Value>, k: &str) -> Result<String, ()> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(()),
    }
}

fn text_part(s: &str) -> Option<Part> {
    (!s.is_empty()).then(|| Part { kind: PART_TEXT.into(), data: go_json(&json!({ "text": s })), ..Part::default() })
}

/// JSON as Go's json.Marshal writes it: `<`, `>`, `&`, U+2028 and U+2029
/// escaped. Those characters only ever occur inside strings, so escaping the
/// encoded text is exact.
fn go_json(v: &Value) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Every turn is "done" and costs nothing: the transcript prices nothing and
/// records no cancellation.
fn turns_from(candidates: &[TurnCandidate]) -> Vec<Turn> {
    split_turns(candidates).iter().map(|_| Turn { status: "done".into(), ..Turn::default() }).collect()
}

/// Newlines before the resume offset — zero whenever the read restarts at
/// zero, under the same conditions read_jsonl rescans on.
fn line_base(file: &mut std::fs::File, held: JsonlCursor, size: u64) -> u64 {
    if held.offset == 0 || held.size > size || held.offset > size || file.seek(SeekFrom::Start(0)).is_err() {
        return 0;
    }
    let mut buf = vec![0u8; 32 << 10];
    let (mut n, mut left, mut r) = (0u64, held.offset, file.take(held.offset));
    while left > 0 {
        let Ok(got) = r.read(&mut buf) else { break };
        if got == 0 {
            break;
        }
        n += buf[..got].iter().filter(|b| **b == b'\n').count() as u64;
        left -= got as u64;
    }
    n
}

/// The slug in `.cursor/projects/<slug>/agent-transcripts/...`, or "".
fn slug_of(ref_path: &str) -> &str {
    let parts: Vec<&str> = ref_path.split('/').collect();
    parts
        .windows(4)
        .find(|w| w[0] == ".cursor" && w[1] == "projects" && !w[2].is_empty() && w[3] == TRANSCRIPTS_DIR)
        .map_or("", |w| w[2])
}

/// The checkout a slug names, trusted only if the decoded directory exists,
/// then walked up for a `.git`. "" when it names nothing here — and for an
/// empty slug, which would otherwise decode to the filesystem root.
fn worktree_of(slug: &str) -> (String, &'static str) {
    if slug.is_empty() {
        return (String::new(), VCS_NONE);
    }
    let decoded = clean(&format!("/{}", slug.replace('-', "/")));
    if !std::fs::metadata(&decoded).is_ok_and(|m| m.is_dir()) {
        return (String::new(), VCS_NONE);
    }
    let mut cur = decoded.as_path();
    loop {
        if std::fs::symlink_metadata(cur.join(".git")).is_ok() {
            return (cur.display().to_string(), VCS_GIT);
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return (decoded.display().to_string(), VCS_NONE),
        }
    }
}

/// Lexical cleaning of a rooted path, as Go's filepath.Clean.
fn clean(p: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in Path::new(p).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

/// The last element of a path, as Go's filepath.Base; "" for "". A
/// `cursor:<slug>` fallback keeps the whole string, naming the slug.
fn base_name(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const MISSING: &str = "22222222-2222-4222-8222-222222222222";
    const REPO_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const MISSING_SLUG: &str = "missing-dir-xyz";
    const FIXTURE_LINES: usize = 7;
    const FIXTURE_MESSAGES: usize = 5;
    const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/cursor");

    /// A temporary directory, removed on drop. Its name holds no dash, so a
    /// slug built from it decodes back exactly.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("gruntascursor{}x{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            assert!(!p.to_string_lossy().contains('-'), "temp dir {p:?} holds a dash; the slug would not decode");
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env_for(home: &Path) -> impl Fn(&str) -> String + use<> {
        let home = home.display().to_string();
        move |k: &str| if k == "HOME" { home.clone() } else { String::new() }
    }

    /// testdata/home copied into a temporary home, with {{SLUG}} renamed to
    /// the slug of a real `work` directory beside it.
    struct Fixture {
        _tmp: Tmp,
        home: PathBuf,
        work: PathBuf,
        slug: String,
    }

    fn fixture() -> Fixture {
        let tmp = Tmp::new();
        let (home, work) = (tmp.0.join("home"), tmp.0.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let slug = work.to_string_lossy().trim_start_matches('/').replace('/', "-");
        fn copy(src: &Path, dst: &Path, slug: &str) {
            std::fs::create_dir_all(dst).unwrap();
            for e in std::fs::read_dir(src).unwrap() {
                let e = e.unwrap();
                let to = dst.join(e.file_name().to_string_lossy().replace("{{SLUG}}", slug));
                if e.file_type().unwrap().is_dir() {
                    copy(&e.path(), &to, slug);
                } else {
                    std::fs::copy(e.path(), to).unwrap();
                }
            }
        }
        copy(&Path::new(TESTDATA).join("home"), &home, &slug);
        Fixture { _tmp: tmp, home, work, slug }
    }

    impl Fixture {
        fn env(&self) -> impl Fn(&str) -> String + use<> {
            env_for(&self.home)
        }
        fn unresolve(&self, s: &str) -> String {
            s.replace(&*self.work.to_string_lossy(), "{{WORK}}").replace(&self.slug, "{{SLUG}}").replace(&*self.home.to_string_lossy(), "{{HOME}}")
        }
        fn refs(&self) -> Vec<Ref> {
            Cursor.discover(&self.env()).unwrap()
        }
        fn get(&self, id: &str) -> Ref {
            self.refs().into_iter().find(|r| r.id == id).unwrap()
        }
        fn read(&self, r: &Ref, cursor: &str) -> (Thread, String, ReadResult) {
            Cursor.read(&self.env(), r, cursor).unwrap()
        }
    }

    /// One transcript at `rel` under a fresh home, with an optional repo.json.
    fn adhoc(rel: &str, id: &str, lines: &[&str], repo_id: &str) -> (Tmp, Ref, PathBuf) {
        let tmp = Tmp::new();
        let full = tmp.0.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        if !repo_id.is_empty() {
            let sidecar = full.parent().unwrap().parent().unwrap().parent().unwrap().join("repo.json");
            std::fs::write(sidecar, format!("{{\"id\": {:?}}}\n", repo_id)).unwrap();
        }
        (tmp, Ref { provider: PROVIDER.into(), id: id.into(), path: rel.into() }, full)
    }

    fn adhoc_path(name: &str) -> String {
        format!("{PROJECTS_DIR}/adhoc-{name}/{TRANSCRIPTS_DIR}/adhoc-{name}-1/adhoc-{name}-1.jsonl")
    }

    fn read_at(tmp: &Tmp, r: &Ref, cursor: &str) -> (Thread, String, ReadResult) {
        Cursor.read(&env_for(&tmp.0), r, cursor).unwrap()
    }

    fn append(path: &Path, line: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(line.as_bytes()).unwrap();
    }

    fn parts_of<'a>(th: &'a Thread, kind: &'a str) -> impl Iterator<Item = &'a Part> + 'a {
        th.messages.iter().flat_map(|m| &m.parts).filter(move |p| p.kind == kind)
    }

    /// The golden's shape: Go's JSON encoding of its canonicalThread.
    fn canonical(f: &Fixture, r: &Ref, th: &Thread, res: &ReadResult) -> Value {
        let map_or_null = |m: &std::collections::BTreeMap<String, usize>| if m.is_empty() { Value::Null } else { json!(m) };
        let mut skipped: Vec<usize> = res.skipped.iter().map(|s| s.line).collect();
        skipped.sort();
        json!({
            "ref": { "provider": r.provider, "id": r.id, "path": f.unresolve(&r.path), "key": r.key() },
            "worktree": { "Path": f.unresolve(&th.worktree.path), "VCS": th.worktree.vcs, "Name": th.worktree.name },
            "session": {
                "directory": f.unresolve(&th.session.directory), "title": th.session.title, "model": th.session.model,
                "provider": th.session.provider, "harness": th.session.harness,
            },
            "binding": {
                "provider": th.binding.provider, "harness": th.binding.harness,
                "foreign_session_id": th.binding.foreign_session_id, "resume_cmd": th.binding.resume_cmd,
            },
            "turns": th.turns.iter().map(|t| json!({ "status": t.status })).collect::<Vec<_>>(),
            "events": th.events.iter().map(|e| json!({ "Type": e.kind, "Data": f.unresolve(&e.data) })).collect::<Vec<_>>(),
            "messages": th.messages.iter().map(|m| json!({
                "role": m.role.as_str(), "provider": m.provider, "model": m.model, "foreign_id": m.foreign_id,
                "usage": m.usage, "raw_json": f.unresolve(m.raw_json.as_deref().unwrap_or("")),
                "parts": m.parts.iter().map(|p| json!({
                    "type": p.kind, "tool_call_id": p.tool_call_id, "signature": p.signature, "data": f.unresolve(&p.data),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "result": {
                "lines": res.lines, "unknown": res.unknown, "unknown_types": map_or_null(&res.unknown_types),
                "classified": map_or_null(&res.classified), "skipped_count": res.skipped_count, "skipped_lines": skipped,
            },
        })
    }

    #[test]
    fn golden() {
        let f = fixture();
        let mut got: Vec<Value> = f
            .refs()
            .iter()
            .map(|r| {
                let (th, _, res) = f.read(r, "");
                canonical(&f, r, &th, &res)
            })
            .collect();
        let want: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(TESTDATA).join("golden.json")).unwrap()).unwrap();
        let mut want = want.as_array().unwrap().clone();
        // Discovery order depends on the temp slug; the discover test pins it.
        let by_id = |v: &Value| v["ref"]["id"].as_str().unwrap().to_string();
        got.sort_by_key(by_id);
        want.sort_by_key(by_id);
        assert_eq!(got, want);
    }

    #[test]
    fn discover_lists_sessions_sorted_by_slug() {
        let f = fixture();
        let got: Vec<String> = f.refs().iter().map(|r| format!("{} {} {}", r.provider, r.id, r.path)).collect();
        let mut want = vec![
            format!("cursor {SESSION} {PROJECTS_DIR}/{}/{TRANSCRIPTS_DIR}/{SESSION}/{SESSION}.jsonl", f.slug),
            format!("cursor {MISSING} {PROJECTS_DIR}/{MISSING_SLUG}/{TRANSCRIPTS_DIR}/{MISSING}/{MISSING}.jsonl"),
        ];
        if MISSING_SLUG < f.slug.as_str() {
            want.swap(0, 1);
        }
        assert_eq!(got, want);
        let refs = f.refs();
        assert_ne!(refs[0].key(), refs[1].key());
    }

    #[test]
    fn discover_on_a_machine_with_no_transcripts() {
        let tmp = Tmp::new();
        assert!(Cursor.discover(&env_for(&tmp.0)).unwrap().is_empty());
        assert!(Cursor.discover(&env_for(&tmp.0.join("nobody"))).unwrap().is_empty());
    }

    #[test]
    fn every_line_is_accounted_for_and_the_sidecar_is_one_event() {
        let f = fixture();
        let (th, _, res) = f.read(&f.get(SESSION), "");
        assert_eq!(res.lines, FIXTURE_LINES);
        assert_eq!(th.messages.len() + res.classified["turn_ended"] + res.skipped_count, FIXTURE_LINES);
        assert_eq!(th.messages.len(), FIXTURE_MESSAGES);
        assert_eq!(res.skipped_count, 1);
        assert!(res.skipped.iter().all(|s| !s.reason.is_empty()));
        assert_eq!(th.events.len(), 1);
        assert_eq!(th.events[0].kind, EVENT_REPO);
        assert!(th.events[0].data.contains(REPO_ID));
        assert_eq!(th.turns.len(), 2, "two prompts, two turns");
        assert!(th.messages.iter().flat_map(|m| &m.parts).all(|p| crate::known_part_type(&p.kind)));
    }

    #[test]
    fn every_tool_result_links_or_is_counted() {
        let f = fixture();
        let (th, _, res) = f.read(&f.get(SESSION), "");
        let calls: HashSet<&str> = parts_of(&th, crate::PART_TOOL_CALL).map(|p| p.tool_call_id.as_str()).collect();
        let results: Vec<&str> = parts_of(&th, PART_TOOL_RESULT).map(|p| p.tool_call_id.as_str()).collect();
        assert_eq!(results.len(), 2);
        assert!(calls.contains(results[0]) && !calls.contains(results[1]));
        assert!(calls.contains("cursor:2"), "the id-less tool_use is keyed by its line");
        assert_eq!(res.classified["tool_result:unlinked"], 1);
    }

    #[test]
    fn worktree_comes_from_the_slug_only_if_it_exists() {
        let f = fixture();
        let (th, _, _) = f.read(&f.get(SESSION), "");
        assert_eq!(th.worktree, Worktree { path: f.work.display().to_string(), vcs: VCS_NONE.into(), name: "work".into() });
        let (missing, _, res) = f.read(&f.get(MISSING), "");
        let fallback = format!("cursor:{MISSING_SLUG}");
        assert_eq!(missing.worktree, Worktree { path: fallback.clone(), vcs: VCS_NONE.into(), name: fallback });
        assert_eq!(res.classified["worktree-fallback"], 1);
        assert!(missing.events.is_empty(), "no sidecar, no event");
    }

    #[test]
    fn worktree_of_detects_git() {
        let tmp = Tmp::new();
        let (git, plain) = (tmp.0.join("git"), tmp.0.join("plain"));
        std::fs::create_dir_all(git.join(".git")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let slug = |p: &Path| p.to_string_lossy().trim_start_matches('/').replace('/', "-");
        assert_eq!(worktree_of(&slug(&git)), (git.display().to_string(), VCS_GIT));
        assert_eq!(worktree_of(&slug(&plain)), (plain.display().to_string(), VCS_NONE));
        assert_eq!(worktree_of(""), (String::new(), VCS_NONE));
    }

    #[test]
    fn read_honors_the_cursor_and_keeps_turns_cumulative() {
        let f = fixture();
        let r = f.get(SESSION);
        let (_, cur, _) = f.read(&r, "");
        let jc = decode_jsonl_cursor(&cur).unwrap();
        let size = std::fs::metadata(f.home.join(&r.path)).unwrap().len();
        assert_eq!((jc.offset, jc.size), (size, size));
        let (delta, _, _) = f.read(&r, &cur);
        assert!(delta.messages.is_empty());
        assert!(delta.events.is_empty(), "the sidecar rides full reads only");
        assert!(Cursor.unchanged(&f.env(), &r, &cur));
        append(&f.home.join(&r.path), "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"third redacted prompt\"}]}}\n");
        assert!(!Cursor.unchanged(&f.env(), &r, &cur));
        let (grown, _, _) = f.read(&r, &cur);
        assert_eq!(grown.messages.len(), 1);
        assert_eq!(grown.turns.len(), 3);
    }

    #[test]
    fn round_trip_through_the_store_inserts_only_what_is_new() {
        let f = fixture();
        let store_home = Tmp::new();
        let conn = gruntas_store::open(&env_for(&store_home.0)).unwrap();
        let w = gruntas_store::Writer::new(&conn);
        let r = f.get(SESSION);
        let (th, cur, _) = f.read(&r, "");
        let first = w.ingest(&th).unwrap();
        assert_eq!((first.messages.inserted, first.events.inserted), (FIXTURE_MESSAGES, 1));
        let (delta, cur2, _) = f.read(&r, &cur);
        let second = w.ingest(&delta).unwrap();
        assert_eq!((second.messages.inserted, second.events.inserted, second.turns.inserted), (0, 0, 0));
        append(&f.home.join(&r.path), "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"third\"}]}}\n");
        let (grown, _, _) = f.read(&r, &cur2);
        let third = w.ingest(&grown).unwrap();
        assert_eq!((third.messages.inserted, third.turns.inserted), (1, 1));
    }

    #[test]
    fn a_missing_transcript_is_an_error() {
        let f = fixture();
        let r = Ref { provider: PROVIDER.into(), id: "gone".into(), path: format!("{PROJECTS_DIR}/gone/{TRANSCRIPTS_DIR}/gone/gone.jsonl") };
        assert!(Cursor.read(&f.env(), &r, "{\"offset\":4096,\"size\":8192}").is_err());
    }

    #[test]
    fn a_ref_with_no_slug_files_under_the_session_id() {
        let (tmp, r, _) = adhoc("odd/path.jsonl", "odd-session-9", &[r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#], "");
        let (th, _, res) = read_at(&tmp, &r, "");
        assert_eq!((th.worktree.path.as_str(), th.worktree.vcs.as_str()), ("cursor:odd-session-9", VCS_NONE));
        assert_eq!(res.classified["worktree-fallback"], 1);
        assert!(th.events.is_empty());
    }

    #[test]
    fn a_system_line_is_a_message_that_opens_no_turn() {
        let (tmp, r, _) = adhoc(
            &adhoc_path("sys"),
            "adhoc-sys-1",
            &[
                r#"{"role":"system","message":{"content":[{"type":"text","text":"first note"}]}}"#,
                r#"{"role":"system","message":{"content":[{"type":"text","text":"second note"}]}}"#,
            ],
            "",
        );
        let (th, _, _) = read_at(&tmp, &r, "");
        assert_eq!(th.messages.len(), 2);
        assert!(th.messages.iter().all(|m| m.role == Role::System && m.parts.len() == 1 && m.parts[0].kind == PART_TEXT));
        assert_eq!(th.turns.len(), 1);
    }

    #[test]
    fn a_shorter_rewrite_rescans_from_zero_and_re_emits_the_sidecar() {
        let repo = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let (tmp, r, path) = adhoc(
            &adhoc_path("rescan"),
            "adhoc-rescan-1",
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"first version one"}]}}"#,
                r#"{"role":"user","message":{"content":[{"type":"text","text":"first version two"}]}}"#,
            ],
            repo,
        );
        let (_, held, _) = read_at(&tmp, &r, "");
        std::fs::write(&path, "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"v2\"}]}}\n").unwrap();
        let (th, _, res) = read_at(&tmp, &r, &held);
        assert_eq!((th.messages.len(), res.lines), (1, 1));
        assert!(th.messages[0].parts[0].data.contains("v2"));
        assert_eq!(th.events.len(), 1);
        assert!(th.events[0].data.contains(repo));
    }

    #[test]
    fn a_mid_line_offset_keeps_absolute_tool_ids() {
        let (tmp, r, _) = adhoc(
            &adhoc_path("mid"),
            "adhoc-mid-1",
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"prompt"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x"}}]}}"#,
            ],
            "",
        );
        let (full, cur, _) = read_at(&tmp, &r, "");
        let want = parts_of(&full, crate::PART_TOOL_CALL).next().unwrap().tool_call_id.clone();
        let jc = decode_jsonl_cursor(&cur).unwrap();
        let mid = encode_cursor(&JsonlCursor { offset: jc.offset - 5, size: jc.size });
        let (again, _, _) = read_at(&tmp, &r, &mid);
        assert_eq!(parts_of(&again, crate::PART_TOOL_CALL).next().unwrap().tool_call_id, want);
        assert_eq!(want, "cursor:2");
    }

    #[test]
    fn a_delta_result_without_its_call_is_unlinked() {
        let (tmp, r, path) =
            adhoc(&adhoc_path("unlinked"), "adhoc-unlinked-1", &[r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x"}}]}}"#], "");
        let (_, held, _) = read_at(&tmp, &r, "");
        append(&path, "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"foreign-xyz\",\"content\":\"late output\"}]}}\n");
        let (delta, _, res) = read_at(&tmp, &r, &held);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(parts_of(&delta, PART_TOOL_RESULT).next().unwrap().tool_call_id, "foreign-xyz");
        assert_eq!(res.classified["tool_result:unlinked"], 1);
    }

    #[test]
    fn two_tool_uses_on_one_line_are_numbered_and_a_bare_string_is_text() {
        let (tmp, r, _) = adhoc(
            &adhoc_path("multiuse"),
            "adhoc-multiuse-1",
            &[
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/a"}},{"type":"tool_use","name":"Write","input":{"path":"/b"}}]}}"#,
                r#"{"role":"assistant","message":{"content":["plain string"]}}"#,
            ],
            "",
        );
        let (th, _, _) = read_at(&tmp, &r, "");
        let calls: Vec<&str> = parts_of(&th, crate::PART_TOOL_CALL).map(|p| p.tool_call_id.as_str()).collect();
        assert_eq!(calls, ["cursor:1", "cursor:1:1"]);
        assert_eq!(th.messages[1].parts, vec![Part { kind: PART_TEXT.into(), data: r#"{"text":"plain string"}"#.into(), ..Part::default() }]);
    }

    #[test]
    fn furniture_type_beats_role() {
        let (tmp, r, _) = adhoc(&adhoc_path("furn"), "adhoc-furn-1", &[r#"{"role":"user","type":"turn_ended"}"#], "");
        let (th, _, res) = read_at(&tmp, &r, "");
        assert!(th.messages.is_empty());
        assert_eq!(res.classified["turn_ended"], 1);
    }

    #[test]
    fn an_empty_text_block_opens_no_turn() {
        let (tmp, r, _) = adhoc(
            &adhoc_path("emptytext"),
            "adhoc-emptytext-1",
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"real prompt"}]}}"#,
                r#"{"role":"user","message":{"content":[{"type":"text","text":""}]}}"#,
            ],
            "",
        );
        let (th, _, _) = read_at(&tmp, &r, "");
        assert_eq!(th.messages.len(), 2);
        assert!(th.messages[1].parts.is_empty());
        assert_eq!(th.turns.len(), 1);
    }

    #[test]
    fn a_null_element_is_unknown_not_empty() {
        let (tmp, r, _) = adhoc(&adhoc_path("null"), "adhoc-null-1", &[r#"{"role":"assistant","message":{"content":[null]}}"#], "");
        let (th, _, res) = read_at(&tmp, &r, "");
        assert_eq!(th.messages[0].parts.len(), 1);
        assert_eq!(th.messages[0].parts[0].kind, crate::PART_UNKNOWN);
        assert_eq!(res.unknown, 1);
    }

    #[test]
    fn a_snake_case_call_id_links() {
        let (tmp, r, _) = adhoc(
            &adhoc_path("snake"),
            "adhoc-snake-1",
            &[
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"tool_result","call_id":"cursor:1","output":"ok"}]}}"#,
            ],
            "",
        );
        let (th, _, res) = read_at(&tmp, &r, "");
        assert_eq!(parts_of(&th, PART_TOOL_RESULT).next().unwrap().tool_call_id, "cursor:1");
        assert!(!res.classified.contains_key("tool_result:unlinked"));
    }

    #[test]
    fn text_is_escaped_as_go_marshals_it() {
        assert_eq!(text_part("<a & b>").unwrap().data, "{\"text\":\"\\u003ca \\u0026 b\\u003e\"}");
        assert_eq!(base_name("/x/work"), "work");
        assert_eq!(base_name("cursor:foo"), "cursor:foo");
        assert_eq!(slug_of(".cursor/projects/s/agent-transcripts/i/i.jsonl"), "s");
        assert_eq!(slug_of("odd/path.jsonl"), "");
    }
}
