//! krowk.db as a projection of the session logs (R-LOG-2): native sessions
//! are one more `Source`, read by the same import, sync and rebuild as
//! Claude's, Cursor's and opencode's transcripts, and written by the same
//! store writer — so they list beside them with harness `krowk` (R-LOG-5),
//! and `krowk sessions rebuild` re-derives them from the JSONL alone.
//!
//! The mapping, event by event, along the head branch:
//!
//! | log                         | krowk.db                                             |
//! |-----------------------------|------------------------------------------------------|
//! | `session.started`           | `session` (directory, worktree) and its `session_binding` (provider and harness `krowk`, foreign id = the session id) |
//! | `turn.started` … `turn.completed` | one `turn`: status, and the token columns summed over the turn's responses |
//! | `item.completed` userText   | a `user` message, one `text` part                     |
//! | `response.completed`        | one `assistant` message holding the items it names: `thinking` (the signature in `part.signature`), `text`, `tool_call` parts |
//! | `item.completed` toolResult | a `tool` message, one `tool_result` part              |
//!
//! Every message's foreign id is the id of the log event it came from, so a
//! re-read of a grown log inserts only what is new. Cost is left to be
//! priced at read time from the token columns, like every other source that
//! reports tokens.

use crate::log;
use crate::protocol::{Item, LogBody, LogEvent, TurnStatus, Usage};
use krowk_import::{encode_cursor, Env, ImportError, JsonlCursor, ReadResult, Ref, Source};
use krowk_store::{Binding, Message, Part, Role, Session, Thread, Turn};
use serde_json::json;
use std::path::Path;

pub const HARNESS: &str = "krowk";

pub struct Krowk;

impl Source for Krowk {
    fn name(&self) -> &'static str {
        HARNESS
    }

    fn discover(&self, env: Env) -> Result<Vec<Ref>, ImportError> {
        let dir = log::sessions_dir(env).map_err(|e| ImportError::NoHome(format!("krowk: {}", e.fix())))?;
        let mut found = log::list(&dir).map_err(|e| ImportError::Other(format!("krowk: list {}: {e}", dir.display())))?;
        // An archived session is its stub until it is restored: listed and
        // found from that alone (R-VINT-3).
        #[cfg(unix)]
        found.extend(crate::vintage::list_archived(&dir));
        Ok(found.into_iter().map(|(id, path)| Ref { provider: HARNESS.into(), id, path: path.display().to_string() }).collect())
    }

    /// The whole log, every time: turns are summed over their responses,
    /// which a read from mid-file could not do. The store dedups messages by
    /// foreign id, so the re-read inserts only what is new.
    fn read(&self, _env: Env, r: &Ref, _cursor: &str) -> Result<(Thread, String, ReadResult), ImportError> {
        let path = Path::new(&r.path);
        let size = std::fs::metadata(path).map_err(|e| ImportError::Other(format!("krowk: stat {}: {e}", r.path)))?.len();
        #[cfg(unix)]
        if path.file_name().is_some_and(|f| f == crate::vintage::STUB_FILE) {
            let stub: crate::vintage::Stub = serde_json::from_slice(&std::fs::read(path).map_err(|e| ImportError::Other(format!("krowk: read {}: {e}", r.path)))?)
                .map_err(|e| ImportError::Other(format!("krowk: {} is not a stub this krowk reads: {e}", r.path)))?;
            return Ok((stub_thread(&r.id, &stub), encode_cursor(&JsonlCursor { offset: size, size }), ReadResult::default()));
        }
        let events = log::read_events(path).map_err(|e| ImportError::Other(format!("krowk: {}", e.message())))?;
        let mut res = ReadResult { lines: events.len(), ..ReadResult::default() };
        let th = thread(&events, &mut res).ok_or_else(|| ImportError::Other(format!("krowk: {} has no session.started event", r.path)))?;
        Ok((th, encode_cursor(&JsonlCursor { offset: size, size }), res))
    }

    fn unchanged(&self, _env: Env, r: &Ref, cursor: &str) -> bool {
        let Ok(c) = krowk_import::decode_jsonl_cursor(cursor) else { return false };
        std::fs::metadata(&r.path).is_ok_and(|m| m.len() == c.size && c.offset == c.size)
    }
}

/// The thread an archived session's stub projects to (R-VINT-3): the
/// session and its turns, each turn with one empty assistant message naming
/// the model it ran on, so it lists, is found and is priced per model as it
/// was — and no bodies, which are in its vintage.
#[cfg(unix)]
pub fn stub_thread(session_id: &str, stub: &crate::vintage::Stub) -> Thread {
    let binding = |id: &str, resume: String| Binding { provider: HARNESS.into(), harness: HARNESS.into(), foreign_session_id: id.into(), resume_cmd: resume };
    Thread {
        worktree: krowk_import::worktree_for(&stub.directory),
        session: Session { directory: stub.directory.clone(), title: stub.title.clone(), model: stub.model.clone(), provider: stub.provider.clone(), harness: HARNESS.into() },
        binding: binding(session_id, format!("krowk -p --resume {session_id}")),
        parent: stub.parent_session_id.as_deref().map(|p| binding(p, String::new())),
        turns: stub
            .turns
            .iter()
            .map(|t| Turn {
                status: t.status.clone(),
                cost_input: t.input,
                cost_output: t.output,
                cost_total: t.total,
                cost_cache_read: t.cache_read,
                cost_cache_write: t.cache_write,
                cost_reasoning: t.reasoning,
                cost_usd_micros: t.usd_micros,
            })
            .collect(),
        messages: stub.turns.iter().enumerate().map(|(i, t)| message(Role::Assistant, &t.provider, &t.model, &format!("archived:{i}"), Some(i as i64), "", Vec::new())).collect(),
        ..Thread::default()
    }
}

/// The thread a log projects to: its head branch, since the listing shows
/// one conversation per session.
pub fn thread(events: &[LogEvent], res: &mut ReadResult) -> Option<Thread> {
    let head = events.last()?.id.clone();
    let branch = log::branch(events, &head);
    let LogBody::SessionStarted { cwd, parent_session_id, .. } = &branch.first()?.body else { return None };
    let session_id = branch[0].session_id.clone();
    let mut th = Thread {
        worktree: krowk_import::worktree_for(cwd),
        session: Session { directory: cwd.clone(), harness: HARNESS.into(), ..Session::default() },
        binding: Binding {
            provider: HARNESS.into(),
            harness: HARNESS.into(),
            foreign_session_id: session_id.clone(),
            resume_cmd: format!("krowk -p --resume {session_id}"),
        },
        // A subagent names the session that spawned it, so krowk.db's
        // session tree — and `krowk sessions budget` — counts its spend there.
        parent: parent_session_id.as_ref().map(|p| Binding { provider: HARNESS.into(), harness: HARNESS.into(), foreign_session_id: p.clone(), resume_cmd: String::new() }),
        ..Thread::default()
    };
    // Items waiting for the response that claims them. Items no response
    // claims — a response that failed mid-stream — are flushed when their
    // turn ends (or the next begins), under that turn: the same message in
    // the same place whether the log is read after that turn or long after,
    // so a rebuild and an incremental import agree (R-LOG-2).
    let mut pending: Vec<(&str, &str, &Item)> = Vec::new();
    let mut provider = String::new();
    let mut turn: Option<i64> = None;
    for ev in &branch {
        res.classify(event_type(&ev.body));
        match &ev.body {
            // The vendor's own record of a backend session stays in the log:
            // krowk.db lists the krowk session, not a second copy of it.
            // So does the run its evidence went to: the registry holds the
            // run, and the tool result that published names it.
            // A subagent is its own session, projected from its own log and
            // linked here by its root's parent; the todo list is the log's,
            // and so are a switch and how a backend was caught up
            // (and the calls that set it are messages already).
            LogBody::SessionStarted { .. } | LogBody::BackendSession { .. } | LogBody::RunOpened { .. } | LogBody::SubagentStarted { .. } | LogBody::TodosUpdated { .. } | LogBody::ModelSwitched { .. } | LogBody::BackendHandoff { .. } => {}
            LogBody::TurnStarted { model, provider: p, .. } => {
                flush(&mut th, &mut pending, &provider, turn);
                provider.clone_from(p);
                th.session.model.clone_from(&model.model);
                th.session.provider.clone_from(p);
                turn = Some(th.turns.len() as i64);
                th.turns.push(Turn { status: "incomplete".into(), ..Turn::default() });
            }
            LogBody::ItemCompleted { item_id, item, .. } => match item {
                Item::UserText { text } => th.messages.push(message(Role::User, "", "", &ev.id, turn, "", vec![text_part(text)])),
                Item::ToolResult { call_id, output, is_error } => th.messages.push(message(
                    Role::Tool,
                    "",
                    "",
                    &ev.id,
                    turn,
                    "",
                    vec![krowk_import::new_tool_result_text_part(call_id, output, *is_error)],
                )),
                _ => pending.push((item_id, &ev.id, item)),
            },
            LogBody::ResponseCompleted { model, usage, item_ids, .. } => {
                let mut parts = Vec::new();
                for id in item_ids {
                    if let Some(at) = pending.iter().position(|(i, _, _)| *i == id.as_str()) {
                        parts.push(part(pending.remove(at).2));
                    }
                }
                let usage_json = serde_json::to_string(usage).expect("usage serializes");
                th.messages.push(message(Role::Assistant, &provider, model, &ev.id, turn, &usage_json, parts));
                if let Some(t) = turn.and_then(|t| th.turns.get_mut(t as usize)) {
                    add_usage(t, usage);
                }
            }
            // A backend's subagent's call: its tokens are the turn's spend,
            // and its conversation is the subagent's.
            LogBody::SubagentResponse { usage, .. } => {
                if let Some(t) = turn.and_then(|t| th.turns.get_mut(t as usize)) {
                    add_usage(t, usage);
                }
            }
            LogBody::TurnCompleted { status, .. } => {
                flush(&mut th, &mut pending, &provider, turn);
                if let Some(t) = turn.and_then(|t| th.turns.get_mut(t as usize)) {
                    t.status = match status {
                        TurnStatus::Completed => "done",
                        TurnStatus::Interrupted => "interrupted",
                        TurnStatus::Failed => "error",
                    }
                    .into();
                }
            }
        }
    }
    // A log that ends mid-turn: its items still say what was produced.
    flush(&mut th, &mut pending, &provider, turn);
    Some(th)
}

/// Each unclaimed item as an assistant message of its own, in log order,
/// keyed by its log event.
fn flush(th: &mut Thread, pending: &mut Vec<(&str, &str, &Item)>, provider: &str, turn: Option<i64>) {
    let model = th.session.model.clone();
    for (_, event_id, item) in pending.drain(..) {
        th.messages.push(message(Role::Assistant, provider, &model, event_id, turn, "", vec![part(item)]));
    }
}

fn event_type(b: &LogBody) -> &'static str {
    match b {
        LogBody::SessionStarted { .. } => "session.started",
        LogBody::TurnStarted { .. } => "turn.started",
        LogBody::ItemCompleted { .. } => "item.completed",
        LogBody::ResponseCompleted { .. } => "response.completed",
        LogBody::TurnCompleted { .. } => "turn.completed",
        LogBody::BackendSession { .. } => "backend.session",
        LogBody::RunOpened { .. } => "run.opened",
        LogBody::SubagentResponse { .. } => "subagent.response",
        LogBody::SubagentStarted { .. } => "subagent.started",
        LogBody::TodosUpdated { .. } => "todos.updated",
        LogBody::ModelSwitched { .. } => "model.switched",
        LogBody::BackendHandoff { .. } => "backend.handoff",
    }
}

fn message(role: Role, provider: &str, model: &str, foreign_id: &str, turn_seq: Option<i64>, usage: &str, parts: Vec<Part>) -> Message {
    Message { role, provider: provider.into(), model: model.into(), foreign_id: foreign_id.into(), usage: usage.into(), raw_json: None, turn_seq, parts }
}

fn text_part(text: &str) -> Part {
    Part { kind: krowk_import::PART_TEXT.into(), data: json!({ "text": text }).to_string(), ..Part::default() }
}

fn part(item: &Item) -> Part {
    match item {
        Item::AssistantText { text } | Item::UserText { text } => text_part(text),
        Item::Reasoning { text, blob } => Part {
            kind: krowk_import::PART_THINKING.into(),
            data: json!({ "thinking": text }).to_string(),
            signature: blob.as_ref().and_then(|b| b.data.get("signature")).and_then(|s| s.as_str()).unwrap_or_default().into(),
            ..Part::default()
        },
        Item::ToolCall { call_id, name, input } => krowk_import::new_tool_call_part(call_id, name, Some(input)),
        Item::ToolResult { call_id, output, is_error } => krowk_import::new_tool_result_text_part(call_id, output, *is_error),
    }
}

/// The store's columns, as every importer fills them: total is every token
/// billed.
fn add_usage(t: &mut Turn, u: &Usage) {
    t.cost_input += u.input_tokens;
    t.cost_output += u.output_tokens;
    t.cost_reasoning += u.reasoning_tokens;
    t.cost_cache_read += u.cache_read_tokens;
    t.cost_cache_write += u.cache_write_tokens;
    t.cost_total += u.total();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ModelRef, PermissionMode, WireApi};

    /// A log as the host writes it: each event hangs from the one before.
    fn log(bodies: Vec<LogBody>) -> Vec<LogEvent> {
        let session = krowk_store::new_id();
        let mut out: Vec<LogEvent> = Vec::new();
        for (i, body) in bodies.into_iter().enumerate() {
            let id = if i == 0 { session.clone() } else { krowk_store::new_id() };
            out.push(LogEvent { id, parent_id: out.last().map(|e| e.id.clone()), session_id: session.clone(), time_ms: 0, body });
        }
        out
    }

    fn turn(t: &str, prompt: &str) -> Vec<LogBody> {
        vec![
            LogBody::TurnStarted {
                turn_id: t.into(),
                model: ModelRef { instance: "anthropic".into(), model: "m".into() },
                provider: "anthropic".into(),
                wire_api: WireApi::AnthropicMessages,
                permission_mode: PermissionMode::Default,
                effort: None,
            },
            LogBody::ItemCompleted { turn_id: t.into(), item_id: format!("{t}-p"), item: Item::UserText { text: prompt.into() } },
        ]
    }

    fn rows(env: &dyn Fn(&str) -> String, th: &Thread) -> Vec<String> {
        let conn = krowk_store::open(env).unwrap();
        krowk_store::Writer::new(&conn).ingest(th).unwrap();
        let id = krowk_store::list_sessions(&conn, "", "", 5).unwrap()[0].id.clone();
        let d = krowk_store::load_session_detail(&conn, &id).unwrap();
        let turns = d.turns.iter().map(|t| format!("turn {} {}", t.seq, t.status));
        let msgs = d.messages.iter().map(|m| format!("{} {} [{}]", m.seq, m.role, m.parts.iter().map(|p| p.data.clone()).collect::<Vec<_>>().join(", ")));
        turns.chain(msgs).collect()
    }

    #[test]
    fn r_log_2_a_turn_that_failed_mid_response_projects_the_same_incrementally_and_on_rebuild() {
        let mut bodies = vec![LogBody::SessionStarted { cwd: "/nowhere".into(), krowk_version: "t".into(), protocol_version: 1, parent_session_id: None, agent: None }];
        bodies.extend(turn("t1", "first"));
        // The response streamed a text item, then failed: no response.completed.
        bodies.push(LogBody::ItemCompleted { turn_id: "t1".into(), item_id: "t1-a".into(), item: Item::AssistantText { text: "half an answer".into() } });
        bodies.push(LogBody::TurnCompleted { turn_id: "t1".into(), status: TurnStatus::Failed, usage: Usage::default(), duration_ms: 1, error: None, reported_cost_usd: None });
        let after_first = bodies.len();
        bodies.extend(turn("t2", "second"));
        bodies.push(LogBody::ItemCompleted { turn_id: "t2".into(), item_id: "t2-a".into(), item: Item::AssistantText { text: "done".into() } });
        bodies.push(LogBody::ResponseCompleted { turn_id: "t2".into(), response_id: None, model: "m".into(), usage: Usage::default(), stop_reason: None, item_ids: vec!["t2-a".into()] });
        bodies.push(LogBody::TurnCompleted { turn_id: "t2".into(), status: TurnStatus::Completed, usage: Usage::default(), duration_ms: 1, error: None, reported_cost_usd: None });
        let events = log(bodies);

        let home = std::env::temp_dir().join(format!("krowk-harness-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let h = home.display().to_string();
        let env = move |k: &str| if k == "HOME" { h.clone() } else { String::new() };
        let project = |evs: &[LogEvent]| thread(evs, &mut ReadResult::default()).unwrap();

        // Incremental: projected after the failed turn, then after the next.
        rows(&env, &project(&events[..after_first]));
        let incremental = rows(&env, &project(&events));
        // Rebuild: the whole log into a fresh store.
        std::fs::remove_file(krowk_store::db_path(&env).unwrap()).unwrap();
        let rebuilt = rows(&env, &project(&events));
        assert_eq!(incremental, rebuilt);
        assert!(rebuilt.iter().any(|r| r.contains("half an answer")), "{rebuilt:?}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
