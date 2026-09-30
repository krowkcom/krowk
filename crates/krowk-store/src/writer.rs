//! Ingest: one thread from one harness, written so a second import of the
//! same transcript converges on the same rows. Dedup is a unique index on
//! foreign ids — (provider, foreign_session_id) for a session, the foreign id
//! for a message — never an id derived from them.

use crate::{clock, other, StoreError};
use rusqlite::{params, Connection, OptionalExtension};

/// Messages are inserted this many to a transaction, so a long transcript
/// does not hold the write lock for the whole import.
const INGEST_BATCH: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
    Error,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
            Role::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Worktree {
    pub path: String,
    pub vcs: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub directory: String,
    pub title: String,
    pub model: String,
    pub provider: String,
    pub harness: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Binding {
    pub provider: String,
    pub harness: String,
    pub foreign_session_id: String,
    pub resume_cmd: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    pub status: String,
    pub cost_input: i64,
    pub cost_output: i64,
    pub cost_total: i64,
    pub cost_cache_read: i64,
    pub cost_cache_write: i64,
    pub cost_reasoning: i64,
    /// Only when the source reports a price; otherwise it is priced at read time.
    pub cost_usd_micros: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Event {
    pub kind: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub provider: String,
    pub model: String,
    pub foreign_id: String,
    pub usage: String,
    pub raw_json: Option<String>,
    /// The seq of the thread turn this message belongs to, when the source
    /// knows it; the message is then linked to that turn's row.
    pub turn_seq: Option<i64>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Part {
    pub kind: String,
    pub tool_call_id: String,
    pub signature: String,
    pub data: String,
    pub foreign_id: String,
}

/// Everything one import writes for one transcript.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Thread {
    pub worktree: Worktree,
    pub session: Session,
    pub binding: Binding,
    pub parent: Option<Binding>,
    pub turns: Vec<Turn>,
    pub events: Vec<Event>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub inserted: usize,
    pub skipped: usize,
}

impl std::ops::AddAssign for Count {
    fn add_assign(&mut self, o: Count) {
        self.inserted += o.inserted;
        self.skipped += o.skipped;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestResult {
    pub worktrees: Count,
    pub sessions: Count,
    pub bindings: Count,
    pub turns: Count,
    pub messages: Count,
    pub parts: Count,
    pub events: Count,
}

pub struct Writer<'a> {
    conn: &'a Connection,
}

fn e(what: &str) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |err| other(&format!("ingest {what}"), err)
}

fn is_retryable(err: &StoreError) -> bool {
    let m = err.message();
    m.contains("UNIQUE constraint failed") || m.contains("is locked")
}

impl<'a> Writer<'a> {
    pub fn new(conn: &'a Connection) -> Writer<'a> {
        Writer { conn }
    }

    /// Writes one thread. A race with another importer surfaces as a unique
    /// violation or a lock, and is retried; anything else is final.
    pub fn ingest(&self, th: &Thread) -> Result<IngestResult, StoreError> {
        if th.worktree.path.is_empty() {
            return Err(StoreError::Other("store: ingest needs a worktree path".into()));
        }
        if th.binding.foreign_session_id.is_empty() {
            return Err(StoreError::Other("store: ingest needs a binding foreign_session_id".into()));
        }
        let now = clock::now_ms();
        let mut total = IngestResult::default();
        let mut last = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(25 * attempt + (now as u64 % 25)));
            }
            match self.ingest_once(th, now) {
                Ok(r) => {
                    add(&mut total, &r);
                    return Ok(total);
                }
                Err(err) if is_retryable(&err) => last = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last.expect("an attempt failed"))
    }

    /// Writes one thread and the import cursor it was read up to, so the next
    /// sync starts where this one stopped.
    pub fn ingest_with_cursor(&self, th: &Thread, key: &str, cursor: &str) -> Result<IngestResult, StoreError> {
        if key.is_empty() {
            return Err(StoreError::Other("store: ingest with cursor needs an import_state key".into()));
        }
        let res = self.ingest(th)?;
        self.conn
            .execute(
                "INSERT INTO import_state (source, cursor, time_updated) VALUES (?, ?, ?)
                 ON CONFLICT(source) DO UPDATE SET cursor = excluded.cursor, time_updated = excluded.time_updated",
                params![key, cursor, clock::now_ms()],
            )
            .map_err(|err| other(&format!("write import state {key:?}"), err))?;
        Ok(res)
    }

    fn ingest_once(&self, th: &Thread, now: i64) -> Result<IngestResult, StoreError> {
        let mut res = IngestResult::default();
        let mut session = th.session.clone();
        session.title = session.title.trim().to_string();
        let from_fallback = session.title.is_empty();
        if from_fallback {
            session.title = title_fallback(&th.messages);
        }

        let tx = self.conn.unchecked_transaction().map_err(e("begin"))?;
        let (worktree_id, inserted) = upsert_worktree(&tx, now, &th.worktree)?;
        *if inserted { &mut res.worktrees.inserted } else { &mut res.worktrees.skipped } += 1;

        let (session_id, created) = find_or_create_session(&tx, now, &worktree_id, &session, &th.binding, from_fallback)?;
        *if created { &mut res.sessions.inserted } else { &mut res.sessions.skipped } += 1;
        *if created { &mut res.bindings.inserted } else { &mut res.bindings.skipped } += 1;

        link_parent(&tx, &session_id, th.parent.as_ref())?;
        res.turns = insert_turn_tail(&tx, now, &session_id, &th.turns)?;
        res.events = insert_event_tail(&tx, now, &session_id, &th.events)?;
        tx.commit().map_err(e("commit"))?;

        let (messages, parts) = self.insert_messages(&session_id, now, &th.messages)?;
        res.messages = messages;
        res.parts = parts;
        Ok(res)
    }

    /// Messages not already stored under their foreign id, appended after the
    /// highest sequence, in batches.
    fn insert_messages(&self, session_id: &str, now: i64, msgs: &[Message]) -> Result<(Count, Count), StoreError> {
        let mut known: std::collections::HashSet<String> = self
            .conn
            .prepare("SELECT foreign_id FROM message WHERE session_id = ? AND foreign_id IS NOT NULL")
            .and_then(|mut s| s.query_map([session_id], |r| r.get::<_, String>(0))?.collect())
            .map_err(e("list foreign ids"))?;
        let max_seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), -1) FROM message WHERE session_id = ?", [session_id], |r| r.get(0))
            .map_err(e("max message seq"))?;
        let (mut messages, mut parts) = (Count::default(), Count::default());
        let mut pending = Vec::new();
        let mut next = max_seq + 1;
        let mut relink = Vec::new();
        for m in msgs {
            if !m.foreign_id.is_empty() && !known.insert(m.foreign_id.clone()) {
                messages.skipped += 1;
                parts.skipped += m.parts.len();
                if let Some(turn) = m.turn_seq {
                    relink.push((&m.foreign_id, turn));
                }
                continue;
            }
            pending.push((m, next));
            next += 1;
        }
        // A message stored before the source named its turn gets the link
        // now, so an upgraded store prices per turn without a rebuild.
        if !relink.is_empty() {
            let tx = self.conn.unchecked_transaction().map_err(e("begin"))?;
            {
                let mut st = tx
                    .prepare(
                        "UPDATE message SET turn_id = (SELECT id FROM turn WHERE session_id = ?1 AND seq = ?2) \
                         WHERE session_id = ?1 AND foreign_id = ?3 AND turn_id IS NULL",
                    )
                    .map_err(e("relink messages"))?;
                for (fid, turn) in relink {
                    st.execute(params![session_id, turn, fid]).map_err(e("relink message"))?;
                }
            }
            tx.commit().map_err(e("commit"))?;
        }
        for chunk in pending.chunks(INGEST_BATCH) {
            let tx = self.conn.unchecked_transaction().map_err(e("begin"))?;
            let mut n_parts = 0;
            for (m, seq) in chunk {
                n_parts += insert_message(&tx, now, session_id, m, *seq)?;
            }
            tx.commit().map_err(e("commit"))?;
            messages.inserted += chunk.len();
            parts.inserted += n_parts;
        }
        Ok((messages, parts))
    }
}

fn add(total: &mut IngestResult, r: &IngestResult) {
    total.worktrees += r.worktrees;
    total.sessions += r.sessions;
    total.bindings += r.bindings;
    total.turns += r.turns;
    total.messages += r.messages;
    total.parts += r.parts;
    total.events += r.events;
}

fn upsert_worktree(tx: &Connection, now: i64, wt: &Worktree) -> Result<(String, bool), StoreError> {
    let found: Option<String> =
        tx.query_row("SELECT id FROM worktree WHERE path = ?", [&wt.path], |r| r.get(0)).optional().map_err(e("find worktree"))?;
    if let Some(id) = found {
        tx.execute("UPDATE worktree SET vcs = ?, name = ?, time_updated = ? WHERE id = ?", params![wt.vcs, wt.name, now, id])
            .map_err(e("update worktree"))?;
        return Ok((id, false));
    }
    let id = clock::new_id();
    tx.execute(
        "INSERT INTO worktree (id, path, vcs, name, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?)",
        params![id, wt.path, wt.vcs, wt.name, now, now],
    )
    .map_err(e("insert worktree"))?;
    Ok((id, true))
}

fn stored_title_or(tx: &Connection, session_id: &str, incoming: &str, from_fallback: bool) -> Result<String, StoreError> {
    let stored: String = tx.query_row("SELECT title FROM session WHERE id = ?", [session_id], |r| r.get(0)).map_err(e("read session title"))?;
    if !from_fallback && !incoming.trim().is_empty() {
        return Ok(incoming.trim().to_string());
    }
    Ok(if stored.is_empty() { incoming.to_string() } else { stored })
}

/// The session this binding already names, updated; else a new session and
/// binding. A title that only came from the fallback never overwrites one the
/// source named.
fn find_or_create_session(
    tx: &Connection,
    now: i64,
    worktree_id: &str,
    s: &Session,
    b: &Binding,
    from_fallback: bool,
) -> Result<(String, bool), StoreError> {
    // Trimmed after the fallback is cut, as Go does: a prompt whose 80th
    // character is a space stores 79.
    let s = &Session { title: s.title.trim().to_string(), ..s.clone() };
    let found: Option<String> = tx
        .query_row(
            "SELECT session_id FROM session_binding WHERE provider = ? AND foreign_session_id = ?",
            params![b.provider, b.foreign_session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(e("find binding"))?;
    if let Some(id) = found {
        let title = if from_fallback {
            stored_title_or(tx, &id, &s.title, true)?
        } else if s.title.is_empty() {
            tx.query_row("SELECT title FROM session WHERE id = ?", [&id], |r| r.get(0)).map_err(e("read session title"))?
        } else {
            s.title.clone()
        };
        tx.execute(
            "UPDATE session SET worktree_id = ?, directory = ?, title = ?, model = ?, provider = ?, harness = ?, time_updated = ? WHERE id = ?",
            params![worktree_id, s.directory, title, s.model, s.provider, s.harness, now, id],
        )
        .map_err(e("update session"))?;
        return Ok((id, false));
    }
    let id = clock::new_id();
    tx.execute(
        "INSERT INTO session (id, worktree_id, directory, title, model, provider, harness, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![id, worktree_id, s.directory, s.title, s.model, s.provider, s.harness, now, now],
    )
    .map_err(e("insert session"))?;
    tx.execute(
        "INSERT INTO session_binding (id, session_id, provider, harness, foreign_session_id, resume_cmd, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![clock::new_id(), id, b.provider, b.harness, b.foreign_session_id, b.resume_cmd, now, now],
    )
    .map_err(e("insert binding"))?;
    Ok((id, true))
}

impl Writer<'_> {
    /// Links a stored subagent session to its parent, now that both may be
    /// stored: a source that lists children before parents (opencode ids
    /// sort newest first) ingests the child while its parent is still
    /// missing, so an import links again once every transcript is in.
    pub fn link_parent_later(&self, child: &Binding, parent: &Binding) -> Result<(), StoreError> {
        let child_id: Option<String> = self
            .conn
            .query_row(
                "SELECT session_id FROM session_binding WHERE provider = ? AND foreign_session_id = ?",
                params![child.provider, child.foreign_session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(e("find child binding"))?;
        match child_id {
            Some(id) => link_parent(self.conn, &id, Some(parent)),
            None => Ok(()),
        }
    }
}

/// A subagent's session points at the one that spawned it, once, and only
/// when that one is already stored.
fn link_parent(tx: &Connection, session_id: &str, parent: Option<&Binding>) -> Result<(), StoreError> {
    let Some(parent) = parent.filter(|p| !p.foreign_session_id.is_empty()) else { return Ok(()) };
    let parent_id: Option<String> = tx
        .query_row(
            "SELECT session_id FROM session_binding WHERE provider = ? AND foreign_session_id = ?",
            params![parent.provider, parent.foreign_session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(e("find parent binding"))?;
    match parent_id {
        Some(pid) if pid != session_id => {
            tx.execute("UPDATE session SET parent_id = ? WHERE id = ? AND parent_id IS NULL", params![pid, session_id]).map_err(e("set parent"))?;
        }
        _ => {}
    }
    Ok(())
}

fn tail(tx: &Connection, table: &str, session_id: &str) -> Result<(usize, i64), StoreError> {
    tx.query_row(&format!("SELECT COUNT(*), COALESCE(MAX(seq), -1) FROM {table} WHERE session_id = ?"), [session_id], |r| {
        Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)?))
    })
    .map_err(e(&format!("count {table}")))
}

/// Turns already stored are skipped by position, and the last stored one is
/// refreshed — a transcript that grew may have finished it.
fn insert_turn_tail(tx: &Connection, now: i64, session_id: &str, turns: &[Turn]) -> Result<Count, StoreError> {
    let (have, max_seq) = tail(tx, "turn", session_id)?;
    let skipped = turns.len().min(have);
    if have > 0 && have as i64 == max_seq + 1 && turns.len() >= have {
        let t = &turns[have - 1];
        tx.execute(
            "UPDATE turn SET status = ?, cost_input_tokens = ?, cost_output_tokens = ?, cost_total_tokens = ?, cost_cache_read_tokens = ?, cost_cache_write_tokens = ?, cost_reasoning_tokens = ?, cost_usd_micros = ?, time_updated = ? WHERE session_id = ? AND seq = ?",
            params![t.status, t.cost_input, t.cost_output, t.cost_total, t.cost_cache_read, t.cost_cache_write, t.cost_reasoning, t.cost_usd_micros, now, session_id, max_seq],
        )
        .map_err(e("refresh turn"))?;
    }
    for (i, t) in turns[skipped..].iter().enumerate() {
        tx.execute(
            "INSERT INTO turn (id, session_id, seq, status, cost_input_tokens, cost_output_tokens, cost_total_tokens, cost_cache_read_tokens, cost_cache_write_tokens, cost_reasoning_tokens, cost_usd_micros, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![clock::new_id(), session_id, max_seq + 1 + i as i64, t.status, t.cost_input, t.cost_output, t.cost_total, t.cost_cache_read, t.cost_cache_write, t.cost_reasoning, t.cost_usd_micros, now, now],
        )
        .map_err(e("insert turn"))?;
    }
    Ok(Count { inserted: turns.len() - skipped, skipped })
}

fn insert_event_tail(tx: &Connection, now: i64, session_id: &str, events: &[Event]) -> Result<Count, StoreError> {
    let (have, max_seq) = tail(tx, "session_event", session_id)?;
    let skipped = events.len().min(have);
    for (i, ev) in events[skipped..].iter().enumerate() {
        let data = if ev.data.is_empty() { "{}" } else { &ev.data };
        tx.execute(
            "INSERT INTO session_event (id, session_id, seq, type, data, time_created) VALUES (?, ?, ?, ?, ?, ?)",
            params![clock::new_id(), session_id, max_seq + 1 + i as i64, ev.kind, data, now],
        )
        .map_err(e("insert event"))?;
    }
    Ok(Count { inserted: events.len() - skipped, skipped })
}

fn none_if_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

fn insert_message(tx: &Connection, now: i64, session_id: &str, m: &Message, seq: i64) -> Result<usize, StoreError> {
    let id = clock::new_id();
    let usage = if m.usage.is_empty() { "{}" } else { &m.usage };
    tx.execute(
        "INSERT INTO message (id, session_id, turn_id, seq, role, provider, model, foreign_id, usage, raw_json, time_created) VALUES (?, ?, (SELECT id FROM turn WHERE session_id = ?2 AND seq = ?), ?, ?, ?, ?, ?, ?, ?, ?)",
        params![id, session_id, m.turn_seq, seq, m.role.as_str(), m.provider, m.model, none_if_empty(&m.foreign_id), usage, m.raw_json, now],
    )
    .map_err(e("insert message"))?;
    for (i, p) in m.parts.iter().enumerate() {
        let data = if p.data.is_empty() { "{}" } else { &p.data };
        tx.execute(
            "INSERT INTO part (id, message_id, session_id, seq, type, tool_call_id, signature, data, foreign_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![clock::new_id(), id, session_id, i as i64, p.kind, none_if_empty(&p.tool_call_id), none_if_empty(&p.signature), data, none_if_empty(&p.foreign_id)],
        )
        .map_err(e("insert part"))?;
    }
    Ok(m.parts.len())
}

/// A session the source named no title for is titled by its first user
/// text, folded to one line and 80 characters.
fn title_fallback(msgs: &[Message]) -> String {
    for m in msgs.iter().filter(|m| m.role == Role::User) {
        for p in m.parts.iter().filter(|p| p.kind == "text") {
            let text = part_text(&p.data).split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                return text.chars().take(80).collect();
            }
        }
    }
    String::new()
}

pub(crate) fn part_text(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data).ok().and_then(|v| v.get("text")?.as_str().map(String::from)).unwrap_or_default()
}

/// The title a thread with no title of its own is listed under: its first
/// prompt, whitespace folded, cut at 80 characters.
pub fn title_for(msgs: &[Message]) -> String {
    title_fallback(msgs)
}

/// Drops a session's bodies — its messages, their parts and its events —
/// and keeps the session row, its bindings and its turns: what listing,
/// search and cost read. An archived session's bodies live in its vintage
/// (R-VINT-3), and a restore drops the archive's stand-ins the same way
/// before the log's own messages go back in. The import cursor goes too,
/// so the next import reads the session afresh.
pub fn drop_bodies(conn: &Connection, provider: &str, foreign_session_id: &str) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction().map_err(e("begin"))?;
    let id: Option<String> = tx
        .query_row("SELECT session_id FROM session_binding WHERE provider = ? AND foreign_session_id = ?", params![provider, foreign_session_id], |r| r.get(0))
        .optional()
        .map_err(e("find binding"))?;
    if let Some(id) = id {
        tx.execute("DELETE FROM message WHERE session_id = ?", [&id]).map_err(e("drop messages"))?;
        tx.execute("DELETE FROM session_event WHERE session_id = ?", [&id]).map_err(e("drop events"))?;
    }
    tx.execute("DELETE FROM import_state WHERE source = ?", [format!("{provider}:{foreign_session_id}")]).map_err(e("drop import state"))?;
    tx.commit().map_err(e("commit"))
}

/// The cursor an import was last read up to, "" when there is none.
pub fn read_import_state(conn: &Connection, key: &str) -> Result<String, StoreError> {
    conn.query_row("SELECT cursor FROM import_state WHERE source = ?", [key], |r| r.get(0))
        .optional()
        .map(Option::unwrap_or_default)
        .map_err(|err| other(&format!("read import state {key:?}"), err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Home;

    fn thread(title: &str, texts: &[&str]) -> Thread {
        Thread {
            worktree: Worktree { path: "/repo".into(), vcs: "git".into(), name: "repo".into() },
            session: Session { title: title.into(), provider: "anthropic".into(), harness: "claude".into(), ..Session::default() },
            binding: Binding { provider: "anthropic".into(), harness: "claude".into(), foreign_session_id: "s1".into(), ..Binding::default() },
            turns: vec![Turn { status: "done".into(), cost_input: 3, ..Turn::default() }],
            messages: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Message {
                    role: Role::User,
                    provider: String::new(),
                    model: String::new(),
                    foreign_id: format!("m{i}"),
                    usage: String::new(),
                    raw_json: None,
                    turn_seq: None,
                    parts: vec![Part { kind: "text".into(), data: serde_json::json!({ "text": t }).to_string(), ..Part::default() }],
                })
                .collect(),
            ..Thread::default()
        }
    }

    #[test]
    fn a_second_import_of_the_same_transcript_converges_and_a_grown_one_appends() {
        let home = Home::new("ingest");
        let env = home.env();
        let conn = crate::open(&env).unwrap();
        let w = Writer::new(&conn);
        let first = w.ingest(&thread("", &["hello   there", "second"])).unwrap();
        assert_eq!((first.sessions.inserted, first.messages.inserted, first.parts.inserted), (1, 2, 2));
        let again = w.ingest(&thread("", &["hello   there", "second"])).unwrap();
        assert_eq!((again.sessions.skipped, again.messages.inserted, again.messages.skipped), (1, 0, 2));
        let grown = w.ingest(&thread("", &["hello   there", "second", "third"])).unwrap();
        assert_eq!(grown.messages.inserted, 1);
        let title: String = conn.query_row("SELECT title FROM session", [], |r| r.get(0)).unwrap();
        assert_eq!(title, "hello there");
        // A title the source names replaces the fallback; the fallback never replaces it back.
        w.ingest(&thread("Named", &["hello"])).unwrap();
        w.ingest(&thread("", &["hello"])).unwrap();
        let title: String = conn.query_row("SELECT title FROM session", [], |r| r.get(0)).unwrap();
        assert_eq!(title, "Named");
        w.ingest_with_cursor(&thread("Named", &["hello"]), "claude:/x", "42").unwrap();
        assert_eq!(read_import_state(&conn, "claude:/x").unwrap(), "42");
        assert_eq!(read_import_state(&conn, "nope").unwrap(), "");
    }
}
