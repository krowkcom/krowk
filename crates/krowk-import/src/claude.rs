//! Claude Code's transcripts, read into the canonical `Thread`.
//!
//! Nothing is lost quietly: every line lands in exactly one of a message, a
//! session event, `ReadResult::classified`, or `ReadResult::skipped`, and a
//! test adds the four up against the line count.
//!
//! - The worktree comes from the lines' `cwd`, never from the directory slug
//!   (`-home-elvinas--buzz` is not invertible): the git toplevel found by
//!   walking up for a `.git` entry, or the directory itself with vcs `none`.
//! - `anthropic` ran the model (session and message provider); `claude`
//!   drove it (harness) and is the binding provider, because that is half of
//!   the persisted session_binding key.
//! - A user-role line is not a prompt when Claude marked it meta, when its
//!   `origin.kind` is set and not `human`, when `promptSource` is `system`,
//!   or when it opens with a tag nothing types — see `injected`.
//! - A subagent transcript (`<slug>/<session>/subagents/agent-<id>.jsonl`)
//!   binds on its agent id and names the dispatching session as its parent;
//!   `discover` lists it right after that session.
//! - `read` always reads from the top: turns are cumulative positional lists
//!   costed over their whole span, so a read from mid-file could neither
//!   number nor cost them. The store dedups messages by foreign id, so the
//!   re-read inserts nothing.

use crate::{
    Env, ImportError, JsonlCursor, LineError, PART_TEXT, PART_THINKING, ReadResult, Ref, Source,
    TurnCandidate, decode_jsonl_cursor, encode_cursor, home_path, jsonl_unchanged,
    new_tool_call_part, new_tool_result_part, open_home, read_jsonl, split_turns,
};
use krowk_store::{Binding, Event, Message, Part, Role, Session, Thread, Turn, Worktree};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

/// The model vendor, on session.provider and every message.provider.
pub const PROVIDER: &str = "anthropic";
/// The tool that produced the transcript.
pub const HARNESS: &str = "claude";

/// Where Claude Code keeps transcripts, relative to home.
const PROJECTS_DIR: &str = ".claude/projects";
/// The directory beside a session's transcript holding its agents' transcripts.
const SUBAGENTS_DIR: &str = "subagents";
/// A subagent transcript's filename is this plus its agent id.
const AGENT_FILE_PREFIX: &str = "agent-";
/// How Claude records a cancelled request — a user-role line that must not
/// open a turn and swallow what was asked next.
const INTERRUPT_PREFIX: &str = "[Request interrupted";
/// The session_event type a hook-carrying attachment lands under.
const EVENT_ATTACHMENT: &str = "attachment";
const VCS_GIT: &str = "git";
const VCS_NONE: &str = "none";

pub struct Claude;

impl Source for Claude {
    fn name(&self) -> &'static str {
        crate::PROVIDER_CLAUDE
    }

    /// Every session transcript, sorted by name, each followed by its
    /// subagents so a caller ingesting in order always has the parent row in
    /// place. A directory that cannot be read is skipped rather than failing
    /// the discovery; no projects directory at all is an empty machine.
    fn discover(&self, env: Env) -> Result<Vec<Ref>, ImportError> {
        crate::check_os().map_err(|e| context(e, "claude"))?;
        let root = home_path(env, PROJECTS_DIR)
            .map_err(|e| context(e, "claude: resolve projects directory"))?;
        let slugs = match sorted_dir(&root) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ImportError::Other(format!("claude: list projects: {e}"))),
        };
        let mut refs = Vec::new();
        for (slug, is_dir) in slugs {
            if !is_dir {
                continue;
            }
            let Ok(files) = sorted_dir(&root.join(&slug)) else {
                continue;
            };
            // A subagents directory can outlive its session file (deleted,
            // rotated away), so directories are struck off as their session
            // is seen and the rest swept up on their own: a subagent
            // transcript is a whole conversation, and its parent link is
            // filled in if the parent ever turns up.
            let mut orphans: Vec<&String> = files
                .iter()
                .filter(|(name, is_dir)| {
                    *is_dir && root.join(&slug).join(name).join(SUBAGENTS_DIR).is_dir()
                })
                .map(|(n, _)| n)
                .collect();
            for (name, is_dir) in &files {
                let Some(session_id) = name.strip_suffix(".jsonl").filter(|_| !is_dir) else {
                    continue;
                };
                orphans.retain(|o| o.as_str() != session_id);
                refs.push(Ref {
                    provider: self.name().into(),
                    id: session_id.into(),
                    path: format!("{PROJECTS_DIR}/{slug}/{name}"),
                });
                refs.extend(subagent_refs(&root, &slug, session_id));
            }
            for session_id in orphans {
                refs.extend(subagent_refs(&root, &slug, session_id));
            }
        }
        Ok(refs)
    }

    /// The whole transcript as one thread. The cursor is checked and then its
    /// offset ignored — see the module doc.
    fn read(
        &self,
        env: Env,
        r: &Ref,
        cursor: &str,
    ) -> Result<(Thread, String, ReadResult), ImportError> {
        decode_jsonl_cursor(cursor).map_err(|e| context(e, "claude"))?;
        let (mut file, _) = open_home(env, &r.path, 0)
            .map_err(|e| context(e, &format!("claude: open {}", r.path)))?;
        let mut b = Builder {
            r: r.clone(),
            subagent: is_subagent_path(&r.path),
            ..Builder::default()
        };
        // Where the transcript lives, for a session that never named a cwd.
        let dir = Path::new(&r.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        b.fallback = home_path(env, if dir.is_empty() { "." } else { &dir })
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let (next, res) = read_jsonl(&mut file, JsonlCursor::default(), |n, raw| b.line(n, raw))
            .map_err(|e| context(e, &format!("claude: read {}", r.path)))?;
        b.acc.merge(&res);
        let th = b.thread();
        Ok((th, encode_cursor(&next), b.acc))
    }

    /// A subagent transcript sits under its session's own directory.
    fn parents(&self, _env: Env, refs: &[Ref]) -> std::collections::HashMap<String, String> {
        refs.iter()
            .filter_map(|r| {
                let mut parts = Path::new(&r.path).components().rev().map(|c| c.as_os_str().to_string_lossy().into_owned());
                let (_file, dir, session) = (parts.next()?, parts.next()?, parts.next()?);
                (dir == SUBAGENTS_DIR).then(|| (r.id.clone(), session))
            })
            .collect()
    }

    fn unchanged(&self, env: Env, r: &Ref, cursor: &str) -> bool {
        jsonl_unchanged(env, r, cursor)
    }
}

/// The same error kind with its message prefixed, so the CLI can still tell a
/// home-sandbox refusal from an I/O failure.
fn context(e: ImportError, prefix: &str) -> ImportError {
    let m = format!("{prefix}: {}", e.message());
    match e {
        ImportError::NoHome(_) => ImportError::NoHome(m),
        ImportError::OutsideHome(_) => ImportError::OutsideHome(m),
        ImportError::EscapingSymlink(_) => ImportError::EscapingSymlink(m),
        ImportError::NotRegularFile(_) => ImportError::NotRegularFile(m),
        ImportError::TooLarge(_) => ImportError::TooLarge(m),
        ImportError::UnsupportedOs(_) => ImportError::UnsupportedOs(m),
        ImportError::Other(_) => ImportError::Other(m),
    }
}

/// A directory's UTF-8 entry names, sorted, with whether each is a directory
/// (not following symlinks).
fn sorted_dir(dir: &Path) -> std::io::Result<Vec<(String, bool)>> {
    let mut out: Vec<(String, bool)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            Some((
                e.file_name().into_string().ok()?,
                e.file_type().is_ok_and(|t| t.is_dir()),
            ))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// One session's subagent transcripts. The agent id comes off the filename so
/// discovery opens nothing; `read` prefers the `agentId` the lines carry.
fn subagent_refs(root: &Path, slug: &str, session_id: &str) -> Vec<Ref> {
    let Ok(files) = sorted_dir(&root.join(slug).join(session_id).join(SUBAGENTS_DIR)) else {
        return Vec::new();
    };
    files
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .filter_map(|(name, _)| {
            let stem = name.strip_suffix(".jsonl")?;
            Some(Ref {
                provider: crate::PROVIDER_CLAUDE.into(),
                id: stem.strip_prefix(AGENT_FILE_PREFIX).unwrap_or(stem).into(),
                path: format!("{PROJECTS_DIR}/{slug}/{session_id}/{SUBAGENTS_DIR}/{name}"),
            })
        })
        .collect()
}

/// Decided from the parent directory's name, before any line is read: a
/// subagent binds on its agent id, a session on its session id.
fn is_subagent_path(path: &str) -> bool {
    Path::new(path)
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == SUBAGENTS_DIR)
}

/// Present-but-null is `Some(Null)`, absent is `None` — the difference Go's
/// json.RawMessage keeps, which decides e.g. whether usage is stored as `null`.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// A JSON null reads as the zero value, as Go decodes it.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

/// One JSONL record, decoded down to the fields acted on; the rest stays in
/// the raw line kept on the message row. A field of the wrong type fails the
/// decode and skips the line, as it does in Go.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Line {
    /// Raw so a missing or non-string type (a skip) is told apart from an
    /// unfamiliar one (a classification).
    #[serde(rename = "type", deserialize_with = "present")]
    kind: Option<Value>,
    #[serde(deserialize_with = "nullable")]
    uuid: String,
    #[serde(rename = "sessionId", deserialize_with = "nullable")]
    session_id: String,
    #[serde(rename = "agentId", deserialize_with = "nullable")]
    agent_id: String,
    #[serde(deserialize_with = "nullable")]
    cwd: String,
    #[serde(rename = "isMeta", deserialize_with = "nullable")]
    is_meta: bool,
    #[serde(rename = "isApiErrorMessage", deserialize_with = "nullable")]
    is_api_error_message: bool,
    /// Where a user line came from: `human`, or an agent talking.
    origin: Option<Origin>,
    #[serde(rename = "promptSource", deserialize_with = "nullable")]
    prompt_source: String,
    message: Option<ApiMessage>,
    /// A `system` line's content: a string everywhere observed, kept raw so
    /// the day it is not costs the content, not the line.
    #[serde(deserialize_with = "present")]
    content: Option<Value>,
    /// Unused beyond the raw line, but typed so a malformed one is a skip.
    #[serde(rename = "subtype", deserialize_with = "nullable")]
    _subtype: String,
    #[serde(deserialize_with = "present")]
    attachment: Option<Value>,
    /// Nested inside `attachment` on every transcript observed; the
    /// documented top-level spelling wins when both are present.
    #[serde(rename = "hookEvent", deserialize_with = "nullable")]
    hook_event: String,
    #[serde(rename = "aiTitle", deserialize_with = "nullable")]
    ai_title: String,
    /// The older title spelling.
    #[serde(deserialize_with = "nullable")]
    summary: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Origin {
    #[serde(deserialize_with = "nullable")]
    kind: String,
}

/// The Anthropic message on a `user` or `assistant` line.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiMessage {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(rename = "role", deserialize_with = "nullable")]
    _role: String,
    #[serde(deserialize_with = "nullable")]
    model: String,
    #[serde(deserialize_with = "present")]
    content: Option<Value>,
    #[serde(deserialize_with = "present")]
    usage: Option<Value>,
}

/// One entry of a message's content array.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ContentBlock {
    #[serde(rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    name: String,
    input: Option<Value>,
    #[serde(deserialize_with = "nullable")]
    tool_use_id: String,
    content: Option<Value>,
    #[serde(deserialize_with = "nullable")]
    is_error: bool,
    #[serde(deserialize_with = "nullable")]
    signature: String,
    #[serde(deserialize_with = "nullable")]
    text: String,
}

/// The token classes a turn is costed from, in Anthropic's names. Read field
/// by field so one malformed class costs only itself, as Go's decode does.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct TokenUsage {
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
}

impl TokenUsage {
    /// Anthropic counts thinking inside `output_tokens` and says how much of
    /// it was thinking in `output_tokens_details.thinking_tokens`; the split
    /// is kept, as every importer keeps it, so a model that ever prices
    /// reasoning apart needs no re-import. Both still price at the output
    /// rate today.
    fn from(v: &Value) -> TokenUsage {
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        let output = n("output_tokens");
        let thinking = v.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_i64).unwrap_or(0).clamp(0, output.max(0));
        TokenUsage {
            input: n("input_tokens"),
            output: output - thinking,
            reasoning: thinking,
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
        }
    }

    /// Total is every token billed: Anthropic reports none, and a reader
    /// adding the columns must get the number the column holds.
    fn add_to(self, t: &mut Turn) {
        t.cost_input += self.input;
        t.cost_output += self.output;
        t.cost_reasoning += self.reasoning;
        t.cost_cache_read += self.cache_read;
        t.cost_cache_write += self.cache_write;
        t.cost_total += self.input + self.output + self.reasoning + self.cache_read + self.cache_write;
    }
}

/// One transcript as it is walked.
#[derive(Default)]
struct Builder {
    r: Ref,
    subagent: bool,
    /// The transcript's own directory: the worktree of a session that never
    /// named one. Not the decoded slug — a wrong guess would file the session
    /// under somebody else's checkout.
    fallback: String,
    acc: ReadResult,
    /// On a subagent file this is the parent's session.
    session_id: String,
    agent_id: String,
    /// First cwd seen: where the session started is the place a person recognises.
    directory: String,
    /// Last title seen: Claude rewrites it as the conversation finds its subject.
    title: String,
    /// Last model named: where the session ended up after any switch.
    model: String,
    messages: Vec<Message>,
    /// Each message's usage and the API message id it belongs to.
    usages: Vec<(String, TokenUsage)>,
    candidates: Vec<TurnCandidate>,
    events: Vec<Event>,
}

impl Builder {
    /// A bad line is a skip; nothing here aborts the file.
    fn line(&mut self, line_no: usize, raw: &[u8]) -> Result<(), LineError> {
        // As Go decodes it (invalid UTF-8, lone surrogates); raw_json is
        // withheld for a line that is not valid UTF-8.
        let v: Value = crate::decode_line(raw)
            .map_err(|e| LineError::Skip(format!("claude: unreadable line: {e}")))?;
        let l: Line = serde_json::from_value(v)
            .map_err(|e| LineError::Skip(format!("claude: unreadable line: {e}")))?;
        let Some(kind) = l
            .kind
            .as_ref()
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return Err(LineError::Skip("claude: line has no string type".into()));
        };
        // Identity and directory come off any line carrying them: a
        // transcript whose only cwd sits on an attachment still ran somewhere.
        for (field, value) in [
            (&mut self.session_id, &l.session_id),
            (&mut self.agent_id, &l.agent_id),
            (&mut self.directory, &l.cwd),
        ] {
            if field.is_empty() {
                field.clone_from(value);
            }
        }
        match kind.as_str() {
            "user" | "assistant" => self.message(&kind, &l, raw, line_no),
            "system" => self.system(&l, raw, line_no),
            "attachment" => self.attachment(&l, &kind),
            "ai-title" | "summary" => {
                let t = if l.ai_title.is_empty() {
                    &l.summary
                } else {
                    &l.ai_title
                };
                if !t.is_empty() {
                    self.title.clone_from(t);
                }
                self.acc.classify(&kind);
            }
            // mode, last-prompt, queue-operation, file-history-*, pr-link,
            // cost-state, agent-name and whatever comes next: classified by
            // name, so an unreleased type is counted rather than unaccounted.
            _ => self.acc.classify(&kind),
        }
        Ok(())
    }

    /// The role is the line type; isApiErrorMessage marks the API's failure,
    /// which must not read as something the model said.
    fn message(&mut self, kind: &str, l: &Line, raw: &[u8], line_no: usize) {
        let role = match (kind, l.is_api_error_message) {
            ("assistant", true) => Role::Error,
            ("assistant", false) => Role::Assistant,
            _ => Role::User,
        };
        let mut msg = Message {
            role,
            provider: PROVIDER.into(),
            model: String::new(),
            foreign_id: self.foreign_id(&l.uuid, line_no),
            usage: String::new(),
            raw_json: raw_json(raw),
            turn_seq: None,
            parts: Vec::new(),
        };
        let mut usage = TokenUsage::default();
        if let Some(m) = &l.message {
            msg.model.clone_from(&m.model);
            // `<synthetic>` is Claude's word for a line no model wrote.
            if !m.model.is_empty() && !m.model.starts_with('<') {
                self.model.clone_from(&m.model);
            }
            if let Some(u) = &m.usage {
                msg.usage = u.to_string();
                usage = TokenUsage::from(u);
            }
            msg.parts = self.parts(m.content.as_ref());
        }
        let text = leading_text(l.message.as_ref());
        self.candidates.push(TurnCandidate {
            role: Some(role),
            meta: l.is_meta || (role == Role::User && injected(l, &text)),
            interrupt: role == Role::User && text.starts_with(INTERRUPT_PREFIX),
            part_types: msg.parts.iter().map(|p| p.kind.clone()).collect(),
            ..TurnCandidate::default()
        });
        self.messages.push(msg);
        self.usages.push((l.message.as_ref().map(|m| m.id.clone()).unwrap_or_default(), usage));
    }

    /// The line's uuid, or one synthesised from the ref and line number. A
    /// message without a foreign id is appended on every ingest (NULL dedups
    /// against nothing); the synthesised one is stable because reads always
    /// start at the top of an append-only file.
    fn foreign_id(&self, uuid: &str, line_no: usize) -> String {
        if uuid.is_empty() {
            format!("{}:line:{line_no}", self.r.id)
        } else {
            uuid.into()
        }
    }

    /// Content is a string on lines typed into the terminal and an array of
    /// blocks elsewhere. Absent, null or "" is no parts — an empty text part
    /// would open a turn nobody prompted.
    fn parts(&mut self, content: Option<&Value>) -> Vec<Part> {
        match content {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) if s.is_empty() => Vec::new(),
            Some(Value::String(s)) => vec![text_part(s)],
            Some(Value::Array(blocks)) => blocks.iter().map(|b| self.block(b)).collect(),
            Some(other) => vec![self.acc.normalize_part("message_content", Some(other))],
        }
    }

    /// tool_use and tool_result go through the contract's constructors so the
    /// call id pairs them; everything else keeps its raw block as data.
    fn block(&mut self, raw: &Value) -> Part {
        let Ok(blk) = ContentBlock::deserialize(raw) else {
            return self.acc.normalize_part("block", Some(raw));
        };
        match blk.kind.as_str() {
            "tool_use" => new_tool_call_part(&blk.id, &blk.name, blk.input.as_ref()),
            "tool_result" => {
                new_tool_result_part(&blk.tool_use_id, blk.content.as_ref(), blk.is_error)
            }
            // Redacted thinking is still thinking; the signature gets its own
            // column because it is what lets the block be replayed.
            PART_THINKING | "redacted_thinking" => {
                let mut part = self.acc.normalize_part(PART_THINKING, Some(raw));
                part.signature = blk.signature;
                part
            }
            other => self.acc.normalize_part(other, Some(raw)),
        }
    }

    /// Claude's own notes (turn durations, API retries) read as transcript,
    /// so they are a system message — always meta, never a prompt.
    fn system(&mut self, l: &Line, raw: &[u8], line_no: usize) {
        let parts = match &l.content {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) if s.is_empty() => Vec::new(),
            Some(Value::String(s)) => vec![text_part(s)],
            Some(other) => vec![self.acc.normalize_part("system_content", Some(other))],
        };
        self.messages.push(Message {
            role: Role::System,
            provider: PROVIDER.into(),
            model: String::new(),
            foreign_id: self.foreign_id(&l.uuid, line_no),
            usage: String::new(),
            raw_json: raw_json(raw),
            turn_seq: None,
            parts,
        });
        self.usages.push((String::new(), TokenUsage::default()));
        self.candidates.push(TurnCandidate {
            role: Some(Role::System),
            meta: true,
            ..TurnCandidate::default()
        });
    }

    /// An attachment is injected context, not a message — importing it as one
    /// would put words in a person's mouth and double the turn count. Only one
    /// carrying a hook event records something that happened, and becomes an
    /// event with the hook and payload; the rest are classified.
    fn attachment(&mut self, l: &Line, kind: &str) {
        let mut hook = l.hook_event.clone();
        if hook.is_empty()
            && let Some(nested) = l
                .attachment
                .as_ref()
                .and_then(|a| a.get("hookEvent"))
                .and_then(Value::as_str)
        {
            hook = nested.into();
        }
        if hook.is_empty() {
            self.acc.classify(kind);
            return;
        }
        let mut data = serde_json::Map::new();
        data.insert("hook_event".into(), json!(hook));
        if let Some(a) = &l.attachment {
            data.insert("attachment".into(), a.clone());
        }
        self.events.push(Event {
            kind: EVENT_ATTACHMENT.into(),
            data: Value::Object(data).to_string(),
        });
    }

    fn thread(&mut self) -> Thread {
        let (mut path, mut vcs) = worktree_of(&self.directory);
        if path.is_empty() {
            (path, vcs) = fallback_worktree(&self.fallback);
        }
        // The file's own agentId wins over the one discovery read off the
        // filename; a transcript with no id at all binds on the ref id, which
        // keeps it agreeing with the import_state key.
        let mut foreign_id = if self.subagent {
            first_non_empty(&self.agent_id, &self.r.id)
        } else {
            self.session_id.clone()
        };
        if foreign_id.is_empty() {
            foreign_id.clone_from(&self.r.id);
        }
        let parent = (self.subagent
            && !self.session_id.is_empty()
            && self.session_id != foreign_id)
            .then(|| Binding {
                provider: crate::PROVIDER_CLAUDE.into(),
                harness: HARNESS.into(),
                foreign_session_id: self.session_id.clone(),
                resume_cmd: String::new(),
            });
        Thread {
            worktree: Worktree {
                name: base_name(&path),
                path,
                vcs: vcs.into(),
            },
            session: Session {
                directory: self.directory.clone(),
                title: self.title.clone(),
                model: self.model.clone(),
                provider: PROVIDER.into(),
                harness: HARNESS.into(),
                ..Session::default()
            },
            binding: Binding {
                provider: crate::PROVIDER_CLAUDE.into(),
                harness: HARNESS.into(),
                foreign_session_id: foreign_id,
                // A subagent cannot be resumed alone; this opens the
                // conversation that dispatched it.
                resume_cmd: resume_cmd(&first_non_empty(&self.session_id, &self.r.id)),
            },
            parent,
            turns: self.turns(),
            events: std::mem::take(&mut self.events),
            messages: {
                let mut messages = std::mem::take(&mut self.messages);
                crate::link_turns(&mut messages, &split_turns(&self.candidates));
                messages
            },
        }
    }

    /// Each turn is costed over its whole span — a tool loop is a dozen API
    /// calls. Status is always "done": the transcript cannot tell a cancelled
    /// turn from a finished one without guessing.
    fn turns(&self) -> Vec<Turn> {
        split_turns(&self.candidates)
            .into_iter()
            .map(|span| {
                let mut t = Turn {
                    status: "done".into(),
                    ..Turn::default()
                };
                // Claude Code writes one line per content block, each
                // repeating its API message's usage, output growing as it
                // streams: a message is counted once, at its largest.
                let mut calls: Vec<(&str, TokenUsage)> = Vec::new();
                for (id, u) in self.usages.iter().take(span.end).skip(span.start) {
                    match calls.iter_mut().find(|(seen, _)| !id.is_empty() && *seen == id.as_str()) {
                        Some((_, best)) if u.output + u.reasoning > best.output + best.reasoning => *best = *u,
                        Some(_) => {}
                        None => calls.push((id, *u)),
                    }
                }
                for (_, u) in calls {
                    u.add_to(&mut t);
                }
                t
            })
            .collect()
    }
}

/// Openings of a user message nothing typed. `<command-name>` and
/// `<bash-input>` are deliberately absent: they are a person using a slash
/// command or bash mode.
const INJECTED_TAGS: [&str; 4] = [
    "<local-command-stdout>",
    "<bash-stdout>",
    "<task-notification>",
    "<system-reminder>",
];

/// Whether a user line came from something other than a person. `origin.kind`
/// is exact; of `promptSource` only `system` is safe — `sdk` is mostly people
/// typing into another front end, and refusing it would drop real prompts.
fn injected(l: &Line, text: &str) -> bool {
    if l.origin
        .as_ref()
        .is_some_and(|o| !o.kind.is_empty() && o.kind != "human")
    {
        return true;
    }
    l.prompt_source == "system" || INJECTED_TAGS.iter().any(|t| text.starts_with(t))
}

/// The start of a user message's prose: the whole string content, or the
/// first non-empty text block.
fn leading_text(msg: Option<&ApiMessage>) -> String {
    match msg.and_then(|m| m.content.as_ref()) {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Array(_)) => Vec::<ContentBlock>::deserialize(v)
            .ok()
            .and_then(|blocks| {
                blocks
                    .into_iter()
                    .find(|b| b.kind == PART_TEXT && !b.text.is_empty())
            })
            .map(|b| b.text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// A text part from a bare string, in the same shape a text block has.
fn text_part(s: &str) -> Part {
    Part {
        kind: PART_TEXT.into(),
        data: json!({ "text": s }).to_string(),
        ..Part::default()
    }
}

/// The source line, or none when it is not text the column could hold.
fn raw_json(raw: &[u8]) -> Option<String> {
    std::str::from_utf8(raw).ok().map(str::to_string)
}

fn resume_cmd(session_id: &str) -> String {
    if session_id.is_empty() {
        String::new()
    } else {
        format!("claude --resume {session_id}")
    }
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.is_empty() { b.into() } else { a.into() }
}

/// Lexical cleaning, as Go's filepath.Clean — a private copy of home.rs's.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// The checkout a session ran in: walk up from its directory for a `.git`
/// entry (a directory, or a linked worktree's file — not followed to the main
/// checkout, which is a different branch). No git is run: that would execute
/// whatever is on PATH per session and answer by the user's config. The walk
/// only stats names, bounded by the root. No `.git` above is not an error:
/// the directory is its own worktree, with vcs `none`.
pub(crate) fn worktree_of(dir: &str) -> (String, &'static str) {
    if dir.is_empty() {
        return (String::new(), VCS_NONE);
    }
    let start = clean(Path::new(dir));
    let mut cur = start.as_path();
    loop {
        if cur.join(".git").symlink_metadata().is_ok() {
            return (cur.display().to_string(), VCS_GIT);
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return (start.display().to_string(), VCS_NONE),
        }
    }
}

/// The worktree of a session with no cwd: the transcript's own directory.
fn fallback_worktree(transcript_dir: &str) -> (String, &'static str) {
    if transcript_dir.is_empty() {
        return (String::new(), VCS_NONE);
    }
    (
        clean(Path::new(transcript_dir)).display().to_string(),
        VCS_NONE,
    )
}

/// The worktree's display name; an empty path has none.
pub(crate) fn base_name(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const AGENT: &str = "a0123456789abcdef";
    const NOGIT_ID: &str = "22222222-2222-4222-8222-222222222222";
    const UNUSED_ID: &str = "44444444-4444-4444-8444-444444444444";
    const SLUG: &str = "-home-elvinas--buzz";
    const HOME_SLUG: &str = "-home-elvinas";
    /// Hand-counted real prompts in the main fixture.
    const PROMPTS: usize = 3;
    /// Hand-counted lines in the main fixture.
    const LINES: usize = 34;
    const TESTDATA: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/claude"
    );

    /// testdata materialised in a temp dir: a home with the transcripts, a
    /// git checkout for them to have run in, and a plain directory. The paths
    /// are real because the worktree rule is a question about the filesystem.
    struct Fixture {
        root: PathBuf,
        home: String,
        real_home: String,
        repo: String,
        no_git: String,
    }

    impl Fixture {
        fn new() -> Fixture {
            static N: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "krowk-claude-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&root);
            let (home, repo, no_git) = (root.join("home"), root.join("repo"), root.join("plain"));
            let sub = repo.join("sub");
            for d in [repo.join(".git"), sub.clone(), no_git.clone()] {
                std::fs::create_dir_all(d).unwrap();
            }
            // testdata spells `.claude` as `dot-claude` so .gitignore does not eat it.
            fn copy(src: &Path, dst: &Path, sub: &str, no_git: &str) {
                std::fs::create_dir_all(dst).unwrap();
                for e in std::fs::read_dir(src).unwrap() {
                    let e = e.unwrap();
                    let name = e.file_name().into_string().unwrap();
                    let to = dst.join(if name == "dot-claude" {
                        ".claude"
                    } else {
                        &name
                    });
                    if e.file_type().unwrap().is_dir() {
                        copy(&e.path(), &to, sub, no_git);
                    } else {
                        let s = std::fs::read_to_string(e.path())
                            .unwrap()
                            .replace("{{CWD}}", sub)
                            .replace("{{NOGIT}}", no_git);
                        std::fs::write(to, s).unwrap();
                    }
                }
            }
            let s = |p: &Path| p.display().to_string();
            copy(
                &Path::new(TESTDATA).join("home"),
                &home,
                &s(&sub),
                &s(&no_git),
            );
            let real_home = s(&home.canonicalize().unwrap());
            Fixture {
                home: s(&home),
                real_home,
                repo: s(&repo),
                no_git: s(&no_git),
                root,
            }
        }

        fn env(&self) -> impl Fn(&str) -> String + '_ {
            move |k| {
                if k == "HOME" {
                    self.home.clone()
                } else {
                    String::new()
                }
            }
        }

        fn discover(&self) -> Vec<Ref> {
            Claude.discover(&self.env()).unwrap()
        }

        fn read(&self, id: &str) -> (Thread, String, ReadResult) {
            let r = self
                .discover()
                .into_iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("no ref {id}"));
            Claude.read(&self.env(), &r, "").unwrap()
        }

        /// Machine paths back to the golden's placeholders.
        fn unresolve(&self, s: &str) -> String {
            s.replace(&self.repo, "{{REPO}}")
                .replace(&self.no_git, "{{NOGIT_DIR}}")
                .replace(&self.real_home, "{{HOME}}")
                .replace(&self.home, "{{HOME}}")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn msg<'a>(th: &'a Thread, id: &str) -> &'a Message {
        th.messages
            .iter()
            .find(|m| m.foreign_id == id)
            .unwrap_or_else(|| panic!("no message {id}"))
    }

    /// The golden's shape: Go's JSON of its canonicalThread, where Worktree
    /// and Event are store structs without tags and so keep Go field names.
    fn canonical(f: &Fixture, r: &Ref, th: &Thread, res: &ReadResult) -> Value {
        let map = |m: &BTreeMap<String, usize>| if m.is_empty() { Value::Null } else { json!(m) };
        let binding = |b: &Binding| json!({"provider": b.provider, "harness": b.harness, "foreign_session_id": b.foreign_session_id, "resume_cmd": b.resume_cmd});
        let mut skipped: Vec<usize> = res.skipped.iter().map(|s| s.line).collect();
        skipped.sort();
        json!({
            "ref": {"provider": r.provider, "id": r.id, "path": r.path, "key": r.key()},
            "worktree": {"Path": f.unresolve(&th.worktree.path), "VCS": th.worktree.vcs, "Name": th.worktree.name},
            "session": {"directory": f.unresolve(&th.session.directory), "title": th.session.title, "model": th.session.model,
                        "provider": th.session.provider, "harness": th.session.harness},
            "binding": binding(&th.binding),
            "parent": th.parent.as_ref().map(binding),
            "turns": th.turns.iter().map(|t| json!({"status": t.status, "cost_input": t.cost_input, "cost_output": t.cost_output,
                "cost_total": t.cost_total, "cost_cache_read": t.cost_cache_read, "cost_cache_write": t.cost_cache_write,
                "cost_reasoning": t.cost_reasoning, "cost_usd_micros": t.cost_usd_micros})).collect::<Vec<_>>(),
            "events": th.events.iter().map(|e| json!({"Type": e.kind, "Data": f.unresolve(&e.data)})).collect::<Vec<_>>(),
            "messages": th.messages.iter().map(|m| json!({"role": m.role.as_str(), "provider": m.provider, "model": m.model,
                "foreign_id": m.foreign_id, "usage": m.usage, "raw_json": f.unresolve(m.raw_json.as_deref().unwrap_or("")),
                "parts": m.parts.iter().map(|p| json!({"type": p.kind, "tool_call_id": p.tool_call_id, "signature": p.signature,
                    "data": f.unresolve(&p.data)})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "result": {"lines": res.lines, "unknown": res.unknown, "unknown_types": map(&res.unknown_types),
                       "classified": map(&res.classified), "skipped_count": res.skipped_count, "skipped_lines": skipped},
        })
    }

    #[test]
    fn golden() {
        let f = Fixture::new();
        let got: Vec<Value> = f
            .discover()
            .iter()
            .map(|r| {
                let (th, _, res) = Claude.read(&f.env(), r, "").unwrap();
                canonical(&f, r, &th, &res)
            })
            .collect();
        let want: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(format!("{TESTDATA}/golden.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(
                g,
                w,
                "\n got: {}\nwant: {}",
                serde_json::to_string_pretty(g).unwrap(),
                serde_json::to_string_pretty(w).unwrap()
            );
        }
    }

    #[test]
    fn discover_lists_sessions_with_their_subagents_after_them() {
        let f = Fixture::new();
        let refs = f.discover();
        let got: Vec<String> = refs
            .iter()
            .map(|r| format!("{} {} {}", r.provider, r.id, r.path))
            .collect();
        let want = [
            format!("claude {UNUSED_ID} .claude/projects/{HOME_SLUG}/{UNUSED_ID}.jsonl"),
            format!("claude {SESSION} .claude/projects/{SLUG}/{SESSION}.jsonl"),
            format!(
                "claude {AGENT} .claude/projects/{SLUG}/{SESSION}/subagents/agent-{AGENT}.jsonl"
            ),
            format!("claude {NOGIT_ID} .claude/projects/-tmp-nogit/{NOGIT_ID}.jsonl"),
        ];
        assert_eq!(got, want);
        assert_ne!(
            refs[1].key(),
            refs[2].key(),
            "a subagent must not share its parent's import_state key"
        );
    }

    #[test]
    fn discover_on_a_machine_with_no_transcripts() {
        let f = Fixture::new();
        std::fs::remove_dir_all(Path::new(&f.home).join(".claude")).unwrap();
        assert!(f.discover().is_empty());
    }

    #[test]
    fn every_line_is_accounted_for() {
        let f = Fixture::new();
        let (th, _, res) = f.read(SESSION);
        let classified: usize = res.classified.values().sum();
        assert_eq!(res.lines, LINES);
        assert_eq!(
            th.messages.len() + th.events.len() + classified + res.skipped_count,
            LINES
        );
        // The line whose type is a number and the one that is not JSON.
        assert_eq!(res.skipped_count, 2);
        assert!(res.skipped.iter().all(|s| !s.reason.is_empty()));
        assert_eq!(res.classified.get("future-thing"), Some(&1));
        assert_eq!(
            (res.unknown, res.unknown_types.get("server_tool_use")),
            (1, Some(&1))
        );
    }

    #[test]
    fn parts_are_canonical_and_every_tool_result_has_its_call() {
        let f = Fixture::new();
        for r in f.discover() {
            let (th, _, _) = Claude.read(&f.env(), &r, "").unwrap();
            let mut calls = std::collections::HashSet::new();
            let mut results = 0;
            for p in th.messages.iter().flat_map(|m| &m.parts) {
                assert!(
                    crate::known_part_type(&p.kind),
                    "{}: part type {:?}",
                    r.id,
                    p.kind
                );
                if p.kind == crate::PART_TOOL_CALL {
                    assert!(!p.tool_call_id.is_empty());
                    calls.insert(p.tool_call_id.clone());
                } else if p.kind == crate::PART_TOOL_RESULT {
                    results += 1;
                    assert!(
                        calls.contains(&p.tool_call_id),
                        "{}: result {:?} has no earlier call",
                        r.id,
                        p.tool_call_id
                    );
                }
            }
            if r.id == SESSION || r.id == AGENT {
                assert!(
                    results > 0,
                    "{}: fixture was meant to hold a tool result",
                    r.id
                );
            }
        }
    }

    #[test]
    fn a_session_that_never_named_a_directory() {
        let f = Fixture::new();
        let (th, _, res) = f.read(UNUSED_ID);
        assert!(th.messages.is_empty() && th.turns.is_empty());
        assert_eq!(
            (
                res.lines,
                res.classified.get("ai-title"),
                res.classified.get("agent-name")
            ),
            (2, Some(&1), Some(&1))
        );
        assert_eq!(th.session.directory, "");
        assert_eq!(
            th.worktree.path,
            format!("{}/{PROJECTS_DIR}/{HOME_SLUG}", f.real_home)
        );
        assert_eq!(th.worktree.vcs, VCS_NONE);
        assert_eq!(th.session.title, "Redacted unused session");
    }

    #[test]
    fn turns_are_the_prompts_plus_the_leading_span_and_sum_the_usage() {
        let f = Fixture::new();
        let (th, _, _) = f.read(SESSION);
        assert_eq!(th.turns.len(), PROMPTS + 1);
        // The expected totals read independently off the raw file.
        let body =
            std::fs::read_to_string(format!("{}/{PROJECTS_DIR}/{SLUG}/{SESSION}.jsonl", f.home))
                .unwrap();
        let mut want = TokenUsage::default();
        for v in body
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let u = &v["message"]["usage"];
            want.input += u["input_tokens"].as_i64().unwrap_or(0);
            want.output += u["output_tokens"].as_i64().unwrap_or(0);
            want.cache_read += u["cache_read_input_tokens"].as_i64().unwrap_or(0);
            want.cache_write += u["cache_creation_input_tokens"].as_i64().unwrap_or(0);
        }
        let mut got = TokenUsage::default();
        for t in &th.turns {
            assert_eq!(t.status, "done");
            assert_eq!((t.cost_reasoning, t.cost_usd_micros), (0, None));
            assert_eq!(
                t.cost_total,
                t.cost_input + t.cost_output + t.cost_reasoning + t.cost_cache_read + t.cost_cache_write
            );
            (got.input, got.output, got.cache_read, got.cache_write) = (
                got.input + t.cost_input,
                got.output + t.cost_output,
                got.cache_read + t.cost_cache_read,
                got.cache_write + t.cost_cache_write,
            );
        }
        assert_eq!(got, want);
        assert_ne!(want, TokenUsage::default());
    }

    #[test]
    fn worktree_comes_from_cwd_not_the_slug() {
        let f = Fixture::new();
        let (th, _, _) = f.read(SESSION);
        assert_eq!(
            (
                th.worktree.path.as_str(),
                th.worktree.vcs.as_str(),
                th.worktree.name.as_str()
            ),
            (f.repo.as_str(), VCS_GIT, "repo")
        );
        assert_eq!(th.session.directory, format!("{}/sub", f.repo));
        let (nogit, _, _) = f.read(NOGIT_ID);
        assert_eq!(
            (nogit.worktree.path.as_str(), nogit.worktree.vcs.as_str()),
            (f.no_git.as_str(), VCS_NONE)
        );
    }

    #[test]
    fn session_names_anthropic_and_binds_on_claude() {
        let f = Fixture::new();
        let (th, _, _) = f.read(SESSION);
        assert_eq!(
            (th.session.provider.as_str(), th.session.harness.as_str()),
            (PROVIDER, HARNESS)
        );
        assert_eq!(
            (
                th.binding.provider.as_str(),
                th.binding.foreign_session_id.as_str()
            ),
            (crate::PROVIDER_CLAUDE, SESSION)
        );
        assert_eq!(th.binding.resume_cmd, format!("claude --resume {SESSION}"));
        assert_eq!(
            th.session.title, "Redacted session title",
            "the last title line wins"
        );
        assert!(th.messages.iter().all(|m| m.provider == PROVIDER));
        assert!(th.parent.is_none());
    }

    #[test]
    fn roles_and_parts_of_the_fixture() {
        let f = Fixture::new();
        let (th, _, res) = f.read(SESSION);
        assert_eq!(
            msg(&th, "cccc0003-0000-4000-8000-000000000003").role,
            Role::Error
        );
        assert_eq!(
            msg(&th, "dddd0001-0000-4000-8000-000000000001").role,
            Role::System
        );
        let thinking = &msg(&th, "cccc0001-0000-4000-8000-000000000001").parts[0];
        assert_eq!(
            (thinking.kind.as_str(), thinking.signature.as_str()),
            (PART_THINKING, "redactedsignature==")
        );
        let text = &msg(&th, "bbbb0002-0000-4000-8000-000000000002").parts[0];
        assert_eq!(
            (text.kind.as_str(), text.data.as_str()),
            (PART_TEXT, r#"{"text":"first redacted prompt"}"#)
        );
        let image = &msg(&th, "bbbb0004-0000-4000-8000-000000000004").parts[1];
        assert!(image.kind == crate::PART_IMAGE && image.data.contains("base64"));
        assert!(
            th.messages
                .iter()
                .all(|m| m.raw_json.as_deref().is_some_and(|r| r.starts_with('{')))
        );
        // Redacted thinking is thinking, not an unknown.
        let redacted = th
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .find(|p| p.data.contains("redacted_thinking"))
            .unwrap();
        assert_eq!(redacted.kind, PART_THINKING);
        assert!(!res.unknown_types.contains_key("redacted_thinking"));
    }

    #[test]
    fn attachments_become_events_only_when_a_hook_fired() {
        let f = Fixture::new();
        let (th, _, res) = f.read(SESSION);
        assert_eq!(th.events.len(), 2);
        assert!(
            th.events
                .iter()
                .all(|e| e.kind == EVENT_ATTACHMENT && e.data.contains(r#""hook_event""#))
        );
        assert!(
            th.events[0].data.contains("SessionStart")
                && th.events[1].data.contains("UserPromptSubmit")
        );
        assert_eq!(res.classified.get("attachment"), Some(&1));
        assert!(th.messages.iter().all(|m| {
            !m.raw_json
                .as_deref()
                .unwrap_or("")
                .contains(r#""type":"attachment""#)
        }));
    }

    #[test]
    fn subagent_binds_on_its_agent_and_names_its_parent() {
        let f = Fixture::new();
        let (th, _, _) = f.read(AGENT);
        assert_eq!(th.binding.foreign_session_id, AGENT);
        let parent = th.parent.expect("a parent binding");
        assert_eq!(
            (parent.provider.as_str(), parent.foreign_session_id.as_str()),
            (crate::PROVIDER_CLAUDE, SESSION)
        );
        assert_eq!(th.binding.resume_cmd, format!("claude --resume {SESSION}"));
        assert_eq!(th.turns.len(), 1);
    }

    #[test]
    fn subagents_are_found_without_their_parent_file() {
        let f = Fixture::new();
        std::fs::remove_file(format!("{}/{PROJECTS_DIR}/{SLUG}/{SESSION}.jsonl", f.home)).unwrap();
        assert!(f.discover().iter().all(|r| r.id != SESSION));
        let (th, _, _) = f.read(AGENT);
        assert_eq!(
            th.parent.map(|p| p.foreign_session_id).as_deref(),
            Some(SESSION)
        );
    }

    #[test]
    fn read_ignores_the_cursor_offset_and_refuses_a_garbled_one() {
        let f = Fixture::new();
        let r = f.discover().into_iter().find(|r| r.id == SESSION).unwrap();
        let (full, cur, _) = Claude.read(&f.env(), &r, "").unwrap();
        let size = std::fs::metadata(format!("{}/{}", f.home, r.path))
            .unwrap()
            .len();
        assert_eq!(
            decode_jsonl_cursor(&cur).unwrap(),
            JsonlCursor { offset: size, size }
        );
        assert!(Claude.unchanged(&f.env(), &r, &cur));
        let (resumed, _, _) = Claude.read(&f.env(), &r, &cur).unwrap();
        assert_eq!(
            (resumed.messages.len(), resumed.turns.len()),
            (full.messages.len(), full.turns.len())
        );
        assert!(Claude.read(&f.env(), &r, "not a cursor").is_err());
        let gone = Ref {
            provider: "claude".into(),
            id: "gone".into(),
            path: format!("{PROJECTS_DIR}/-gone/gone.jsonl"),
        };
        assert!(Claude.read(&f.env(), &gone, &cur).is_err());
    }

    #[test]
    fn a_line_with_no_uuid_gets_a_stable_foreign_id() {
        let f = Fixture::new();
        let (th, _, _) = f.read(SESSION);
        assert!(th.messages.iter().all(|m| !m.foreign_id.is_empty()));
        let synth: Vec<_> = th
            .messages
            .iter()
            .filter(|m| m.foreign_id.starts_with(&format!("{SESSION}:line:")))
            .collect();
        assert_eq!(synth.len(), 1);
        let (again, _, _) = f.read(SESSION);
        assert_eq!(
            again.messages.last().unwrap().foreign_id,
            th.messages.last().unwrap().foreign_id
        );
    }

    #[test]
    fn null_content_and_task_notifications_open_no_turn() {
        let f = Fixture::new();
        let (th, _, _) = f.read(SESSION);
        assert!(
            msg(&th, "bbbb0007-0000-4000-8000-000000000007")
                .parts
                .is_empty()
        );
        let note = msg(&th, "bbbb0008-0000-4000-8000-000000000008");
        assert_eq!(
            (note.role, note.parts.len()),
            (Role::User, 1),
            "still a message: the notification is transcript"
        );
        assert_eq!(th.turns.len(), PROMPTS + 1);
    }

    #[test]
    fn empty_content_is_no_parts() {
        let mut b = Builder::default();
        for c in [None, Some(Value::Null), Some(json!(""))] {
            assert!(b.parts(c.as_ref()).is_empty());
        }
        let p = b.parts(Some(&json!("hello")));
        assert!(p.len() == 1 && p[0].kind == PART_TEXT);
    }

    #[test]
    fn injected_line_rule() {
        let line = |source: &str, origin: Option<&str>| Line {
            prompt_source: source.into(),
            origin: origin.map(|k| Origin { kind: k.into() }),
            ..Line::default()
        };
        let cases = [
            (
                "a person typing",
                line("typed", Some("human")),
                "merge",
                false,
            ),
            (
                "a person through the sdk",
                line("sdk", None),
                "Good to ship?",
                false,
            ),
            (
                "a slash command",
                line("sdk", None),
                "<command-name>/review</command-name>",
                false,
            ),
            (
                "bash mode",
                line("", None),
                "<bash-input>ls</bash-input>",
                false,
            ),
            (
                "a queued human prompt",
                line("queued", Some("human")),
                "push",
                false,
            ),
            ("an old line", line("", None), "what is this", false),
            (
                "an agent reporting back",
                line("sdk", Some("task-notification")),
                "<task-notification>done</task-notification>",
                true,
            ),
            (
                "a coordinator",
                line("", Some("coordinator")),
                "The coordinator sent a message",
                true,
            ),
            (
                "a peer agent",
                line("", Some("peer")),
                "please re-run",
                true,
            ),
            (
                "the system prompting",
                line("system", None),
                "anything",
                true,
            ),
            (
                "command output",
                line("", None),
                "<local-command-stdout>ok</local-command-stdout>",
                true,
            ),
            (
                "bash output",
                line("", None),
                "<bash-stdout>ok</bash-stdout>",
                true,
            ),
        ];
        for (name, l, text, want) in cases {
            assert_eq!(injected(&l, text), want, "{name}");
        }
    }

    #[test]
    fn system_content_that_is_not_a_string_is_not_a_skip() {
        let mut b = Builder::default();
        b.system(
            &Line {
                uuid: "u1".into(),
                content: Some(json!({"text": "structured"})),
                ..Line::default()
            },
            b"{}",
            1,
        );
        let p = &b.messages[0].parts;
        assert!(
            p.len() == 1 && p[0].kind == crate::PART_UNKNOWN && p[0].data.contains("structured")
        );
    }

    fn open_store(name: &str) -> (PathBuf, rusqlite::Connection) {
        let home =
            std::env::temp_dir().join(format!("krowk-claude-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let h = home.display().to_string();
        let conn = krowk_store::open(&move |k: &str| {
            if k == "HOME" {
                h.clone()
            } else {
                String::new()
            }
        })
        .unwrap();
        (home, conn)
    }

    #[test]
    fn ingesting_the_fixture_twice_inserts_nothing_the_second_time() {
        let f = Fixture::new();
        let (dir, conn) = open_store("twice");
        let w = krowk_store::Writer::new(&conn);
        let pass = || {
            let mut total = krowk_store::IngestResult::default();
            for r in f.discover() {
                let (th, _, _) = Claude.read(&f.env(), &r, "").unwrap();
                let res = w.ingest(&th).unwrap();
                for (t, c) in [
                    (&mut total.worktrees, res.worktrees),
                    (&mut total.sessions, res.sessions),
                    (&mut total.bindings, res.bindings),
                    (&mut total.turns, res.turns),
                    (&mut total.events, res.events),
                    (&mut total.messages, res.messages),
                    (&mut total.parts, res.parts),
                ] {
                    t.inserted += c.inserted;
                }
            }
            total
        };
        let first = pass();
        assert!(
            first.messages.inserted > 0 && first.turns.inserted > 0 && first.events.inserted > 0
        );
        let second = pass();
        let inserted = [
            second.worktrees,
            second.sessions,
            second.bindings,
            second.turns,
            second.events,
            second.messages,
            second.parts,
        ];
        assert!(
            inserted.iter().all(|c| c.inserted == 0),
            "re-importing inserted {second:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn importing_a_live_session_converges_on_the_final_costs() {
        let costs = |conn: &rusqlite::Connection| -> Vec<(i64, i64)> {
            let mut q = conn
                .prepare(
                    "SELECT t.cost_input_tokens, t.cost_total_tokens FROM turn t
                     JOIN session_binding b ON b.session_id = t.session_id
                     WHERE b.provider = ? AND b.foreign_session_id = ? ORDER BY t.seq",
                )
                .unwrap();
            q.query_map([crate::PROVIDER_CLAUDE, SESSION], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        };
        let whole = Fixture::new();
        let (d1, db1) = open_store("whole");
        krowk_store::Writer::new(&db1)
            .ingest(&whole.read(SESSION).0)
            .unwrap();
        let want = costs(&db1);
        assert_eq!(want.len(), PROMPTS + 1);

        let live = Fixture::new();
        let (d2, db2) = open_store("live");
        let path = format!("{}/{PROJECTS_DIR}/{SLUG}/{SESSION}.jsonl", live.home);
        let full = std::fs::read_to_string(&path).unwrap();
        // Seven lines is partway through the first prompt's tool loop.
        let cut: String = full.split_inclusive('\n').take(7).collect();
        std::fs::write(&path, cut).unwrap();
        krowk_store::Writer::new(&db2)
            .ingest(&live.read(SESSION).0)
            .unwrap();
        let mid = costs(&db2);
        assert!(
            mid.len() == 2 && mid[1].1 != want[1].1,
            "not a session caught mid-turn: {mid:?}"
        );
        std::fs::write(&path, full).unwrap();
        krowk_store::Writer::new(&db2)
            .ingest(&live.read(SESSION).0)
            .unwrap();
        assert_eq!(costs(&db2), want);
        let _ = (std::fs::remove_dir_all(d1), std::fs::remove_dir_all(d2));
    }

    #[test]
    fn each_message_names_its_turn_so_a_model_switch_prices_per_turn() {
        let dir = std::env::temp_dir().join(format!("krowk-claude-turns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let slug = dir.join("home/.claude/projects/-work");
        std::fs::create_dir_all(&slug).unwrap();
        std::fs::create_dir_all(dir.join("home/work")).unwrap();
        let home = dir.join("home").canonicalize().unwrap().display().to_string();
        let cwd = format!("{home}/work");
        let sid = "55555555-5555-4555-8555-555555555555";
        let line = |kind: &str, uuid: &str, message: Value| json!({ "type": kind, "uuid": uuid, "sessionId": sid, "cwd": cwd, "message": message }).to_string();
        let asst = |model: &str, id: &str| json!({ "id": id, "role": "assistant", "model": model, "content": [{ "type": "text", "text": "ok" }], "usage": { "input_tokens": 1, "output_tokens": 1 } });
        let body = [
            line("user", "u1", json!({ "role": "user", "content": "first" })),
            line("assistant", "a1", asst("claude-opus-5", "msg_1")),
            line("user", "u2", json!({ "role": "user", "content": "second" })),
            line("assistant", "a2", asst("claude-sonnet-5", "msg_2")),
            line("assistant", "a3", asst("<synthetic>", "msg_3")),
        ]
        .join("\n");
        std::fs::write(slug.join(format!("{sid}.jsonl")), body + "\n").unwrap();
        let env = move |k: &str| if k == "HOME" { home.clone() } else { String::new() };
        let r = Claude.discover(&env).unwrap().into_iter().next().unwrap();
        let (th, _, _) = Claude.read(&env, &r, "").unwrap();
        assert_eq!(th.turns.len(), 2);
        let links: Vec<(Option<i64>, &str)> = th.messages.iter().map(|m| (m.turn_seq, m.model.as_str())).collect();
        assert_eq!(links, vec![(Some(0), ""), (Some(0), "claude-opus-5"), (Some(1), ""), (Some(1), "claude-sonnet-5"), (Some(1), "<synthetic>")]);
        assert_eq!(th.session.model, "claude-sonnet-5", "a synthetic line names no session model");
        // msg_2 streams as two more lines of one call: counted once, at its end.
        let more = [
            line("assistant", "a4", json!({ "id": "msg_2", "role": "assistant", "model": "claude-sonnet-5", "content": [{ "type": "tool_use", "id": "t1", "name": "Bash", "input": {} }], "usage": { "input_tokens": 1, "output_tokens": 40, "output_tokens_details": { "thinking_tokens": 30 } } })),
            line("assistant", "a5", json!({ "id": "msg_2", "role": "assistant", "model": "claude-sonnet-5", "content": [{ "type": "text", "text": "x" }], "usage": { "input_tokens": 1, "output_tokens": 40, "output_tokens_details": { "thinking_tokens": 30 } } })),
        ]
        .join("\n");
        let path = slug.join(format!("{sid}.jsonl"));
        let body = std::fs::read_to_string(&path).unwrap() + &more + "\n";
        std::fs::write(&path, body).unwrap();
        let (th, _, _) = Claude.read(&env, &r, "").unwrap();
        let t = &th.turns[1];
        assert_eq!((t.cost_input, t.cost_output, t.cost_reasoning), (1 + 1, 1 + 10, 30), "msg_2 once at 40 (30 thinking), msg_3 once");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
