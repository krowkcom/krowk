//! Reading threads back: the listing, one session in full, and the id a
//! caller's reference resolves to.

use crate::{other, StoreError};
use rusqlite::{Connection, OptionalExtension, Row};
use std::collections::HashMap;

const SESSION_LIST: &str = "WITH page AS (SELECT s.id AS pid FROM session s LEFT JOIN worktree w ON w.id = s.worktree_id LEFT JOIN (SELECT session_id, MIN(id) AS id FROM session_binding GROUP BY session_id) one ON one.session_id = s.id LEFT JOIN session_binding b ON b.id = one.id WHERE (?1 = '' OR COALESCE(b.harness, s.harness) = ?1) AND (?2 = '' OR w.path = ?2) ORDER BY s.time_updated DESC LIMIT ?3) SELECT s.id, s.title, s.model, s.provider, s.harness, s.directory, s.time_created, s.time_updated, w.path, b.harness, b.provider, b.foreign_session_id, COALESCE(t.n, 0), COALESCE(t.sum_in, 0), COALESCE(t.sum_out, 0), COALESCE(t.sum_total, 0), COALESCE(t.sum_cread, 0), COALESCE(t.sum_cwrite, 0), COALESCE(t.sum_reason, 0) FROM session s JOIN page ON page.pid = s.id LEFT JOIN worktree w ON w.id = s.worktree_id LEFT JOIN (SELECT session_id, MIN(id) AS id FROM session_binding GROUP BY session_id) one ON one.session_id = s.id LEFT JOIN session_binding b ON b.id = one.id LEFT JOIN (SELECT session_id, COUNT(*) AS n, SUM(cost_input_tokens) AS sum_in, SUM(cost_output_tokens) AS sum_out, SUM(cost_total_tokens) AS sum_total, SUM(cost_cache_read_tokens) AS sum_cread, SUM(cost_cache_write_tokens) AS sum_cwrite, SUM(cost_reasoning_tokens) AS sum_reason FROM turn WHERE session_id IN (SELECT pid FROM page) GROUP BY session_id) t ON t.session_id = s.id ORDER BY s.time_updated DESC";

pub const DEFAULT_SESSION_PAGE: i64 = 50;

/// The (provider, model) a turn is priced by: its last assistant message
/// naming a real model, else the session's. Claude writes `<synthetic>` on
/// messages no model produced, which name no price. A scalar subquery per
/// turn, answered from idx_message_turn alone (it covers every column read
/// here), so a page of sessions reads only its own turns and no message rows.
const TURN_MODEL_FROM: &str = "FROM message m WHERE m.turn_id = t.id AND m.role = 'assistant' AND m.model != '' AND m.model NOT LIKE '<%>' ORDER BY m.seq DESC LIMIT 1";

/// A session's turns summed per (provider, model) — the pair a price is
/// looked up by — and split by whether the source reported the dollars.
/// Ledger turns something else already accounts for — a transcript, or an
/// earlier export — are left out: they count once, there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostGroup {
    pub provider: String,
    pub model: String,
    /// The source stated a cost above 0 for these turns; `usd_micros` is its
    /// sum. opencode writes 0 for a model it cannot price, so a stated 0 is
    /// no statement — those turns are priced from their tokens instead.
    pub reported: bool,
    pub turns: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub usd_micros: i64,
}

/// Cost groups for the given sessions, keyed by session id. With no ids,
/// every session in the store.
pub fn cost_groups(conn: &Connection, session_ids: &[String]) -> Result<HashMap<String, Vec<CostGroup>>, StoreError> {
    let base = format!(
        "SELECT t.session_id, COALESCE((SELECT m.provider {TURN_MODEL_FROM}), s.provider), COALESCE((SELECT m.model {TURN_MODEL_FROM}), s.model), \
         COALESCE(t.cost_usd_micros, 0) > 0, COUNT(*), SUM(t.cost_input_tokens), SUM(t.cost_output_tokens), SUM(t.cost_cache_read_tokens), \
         SUM(t.cost_cache_write_tokens), SUM(t.cost_reasoning_tokens), COALESCE(SUM(CASE WHEN t.cost_usd_micros > 0 THEN t.cost_usd_micros END), 0) \
         FROM turn t JOIN session s ON s.id = t.session_id WHERE t.status NOT IN ('{observed}', '{duplicate}')",
        observed = crate::STATUS_OBSERVED,
        duplicate = crate::STATUS_DUPLICATE,
    );
    let mut out: HashMap<String, Vec<CostGroup>> = HashMap::new();
    let mut run = |sql: &str, ids: &[String]| -> Result<(), StoreError> {
        let mut st = conn.prepare(sql).map_err(|e| other("cost groups", e))?;
        let rows = st
            .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    CostGroup {
                        provider: string(r, 1)?,
                        model: string(r, 2)?,
                        reported: r.get(3)?,
                        turns: r.get(4)?,
                        input: r.get(5)?,
                        output: r.get(6)?,
                        cache_read: r.get(7)?,
                        cache_write: r.get(8)?,
                        reasoning: r.get(9)?,
                        usd_micros: r.get(10)?,
                    },
                ))
            })
            .map_err(|e| other("cost groups", e))?;
        for row in rows {
            let (sid, g) = row.map_err(|e| other("scan cost group", e))?;
            out.entry(sid).or_default().push(g);
        }
        Ok(())
    };
    const GROUP_BY: &str = " GROUP BY 1, 2, 3, 4 ORDER BY 1, 2, 3, 4";
    if session_ids.is_empty() {
        run(&format!("{base}{GROUP_BY}"), &[])?;
    }
    // Chunked under SQLite's bound-parameter limit.
    for chunk in session_ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        run(&format!("{base} AND t.session_id IN ({marks}){GROUP_BY}"), chunk)?;
    }
    Ok(out)
}


/// One row of the listing: the session, its first binding, and its turns summed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub model: String,
    pub provider: String,
    /// The first binding's harness when there is one, else the session's.
    pub harness: String,
    pub directory: String,
    pub time_created: i64,
    pub time_updated: i64,
    pub worktree_path: String,
    pub binding_harness: String,
    pub binding_provider: String,
    pub foreign_session_id: String,
    pub turn_count: i64,
    pub sum_input: i64,
    pub sum_output: i64,
    pub sum_total: i64,
    pub sum_cache_read: i64,
    pub sum_cache_write: i64,
    pub sum_reasoning: i64,
}

fn string(r: &Row, i: usize) -> rusqlite::Result<String> {
    Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
}

/// Newest first. `limit` 0 is the default page, -1 is everything.
pub fn list_sessions(conn: &Connection, harness: &str, worktree: &str, limit: i64) -> Result<Vec<SessionRow>, StoreError> {
    let limit = match limit {
        0 => DEFAULT_SESSION_PAGE,
        -1 => -1,
        n if n > 0 => n,
        n => {
            return Err(StoreError::Other(format!(
                "store: list sessions: bad limit {n} (want -1 for all, 0 for the default page, or a positive page)"
            )))
        }
    };
    let mut stmt = conn.prepare(SESSION_LIST).map_err(|e| other("list sessions", e))?;
    let rows = stmt
        .query_map(rusqlite::params![harness, worktree, limit], |r| {
            let mut row = SessionRow {
                id: r.get(0)?,
                title: r.get(1)?,
                model: r.get(2)?,
                provider: r.get(3)?,
                harness: r.get(4)?,
                directory: r.get(5)?,
                time_created: r.get(6)?,
                time_updated: r.get(7)?,
                worktree_path: string(r, 8)?,
                binding_harness: string(r, 9)?,
                binding_provider: string(r, 10)?,
                foreign_session_id: string(r, 11)?,
                turn_count: r.get(12)?,
                sum_input: r.get(13)?,
                sum_output: r.get(14)?,
                sum_total: r.get(15)?,
                sum_cache_read: r.get(16)?,
                sum_cache_write: r.get(17)?,
                sum_reasoning: r.get(18)?,
            };
            if !row.binding_harness.is_empty() {
                row.harness = row.binding_harness.clone();
            }
            Ok(row)
        })
        .map_err(|e| other("list sessions", e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| other("scan session row", e))
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The foreign id `provider` knows the session `session_id` by, if it has
/// a binding there: for a native session stored before its row took its
/// log's id, the log id `krowk sync host` and `--resume` need.
pub fn foreign_session_id(conn: &Connection, session_id: &str, provider: &str) -> Result<Option<String>, StoreError> {
    conn.query_row(
        "SELECT foreign_session_id FROM session_binding WHERE session_id = ? AND provider = ? ORDER BY id LIMIT 1",
        [session_id, provider],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| other("find foreign session id", e))
}

/// A caller's reference as one session id: the id itself, a foreign session
/// id, or an id prefix of at least 8 characters. Several matches are refused
/// by name, never guessed between.
pub fn resolve_session_id(conn: &Connection, reference: &str) -> Result<String, StoreError> {
    let r = reference.trim();
    if r.is_empty() {
        return Err(StoreError::NotFound("store: pass a session id: `krowk sessions show <id>`".into()));
    }
    if let Some(id) = conn.query_row("SELECT id FROM session WHERE id = ?", [r], |row| row.get::<_, String>(0)).optional().map_err(|e| other("resolve session", e))? {
        return Ok(id);
    }
    let bindings: Vec<(String, String)> = conn
        .prepare("SELECT session_id, provider FROM session_binding WHERE foreign_session_id = ? ORDER BY provider, session_id")
        .and_then(|mut s| s.query_map([r], |row| Ok((row.get(0)?, row.get(1)?)))?.collect())
        .map_err(|e| other("resolve session", e))?;
    let mut unique: Vec<(String, String)> = Vec::new();
    for (sid, provider) in bindings {
        if !unique.iter().any(|(u, _)| *u == sid) {
            unique.push((sid, provider));
        }
    }
    match unique.len() {
        0 => {}
        1 => return Ok(unique.remove(0).0),
        n => {
            let mut msg = format!("store: {r:?} is ambiguous ({n} sessions):");
            for (sid, provider) in &unique {
                msg += &format!("\n  {sid}  {provider}");
            }
            return Err(StoreError::Ambiguous { message: msg, ids: unique.into_iter().map(|(s, _)| s).collect() });
        }
    }
    if r.chars().count() < 8 {
        return Err(StoreError::NotFound(format!(
            "store: {r:?} matches no session — pass a full id, an id prefix of at least 8 chars, or a foreign session id"
        )));
    }
    let candidates: Vec<(String, String)> = conn
        .prepare("SELECT id, title FROM session WHERE id LIKE ? ESCAPE '\\' ORDER BY id LIMIT 11")
        .and_then(|mut s| s.query_map([format!("{}%", escape_like(r))], |row| Ok((row.get(0)?, row.get(1)?)))?.collect())
        .map_err(|e| other("resolve session", e))?;
    match candidates.len() {
        0 => Err(StoreError::NotFound(format!("store: {r:?} matches no session"))),
        1 => Ok(candidates[0].0.clone()),
        n => {
            let mut msg = if n > 10 {
                format!("store: {r:?} is ambiguous (more than 10 sessions, showing first 10 — refine the prefix):")
            } else {
                format!("store: {r:?} is ambiguous ({n} sessions):")
            };
            let shown: Vec<&(String, String)> = candidates.iter().take(10).collect();
            for (id, title) in &shown {
                let title = cell(title);
                msg += &format!("\n  {id}  {}", if title.is_empty() { "(untitled)" } else { &title });
            }
            Err(StoreError::Ambiguous { message: msg, ids: shown.into_iter().map(|(id, _)| id.clone()).collect() })
        }
    }
}

/// A caller's text folded to one terminal-safe row: an escape sequence or a
/// bidi override in a title must not repaint the ambiguity list.
fn cell(s: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            space = true;
        } else if c.is_control() || matches!(c, '\u{feff}' | '\u{200b}'..='\u{200c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnDetail {
    pub seq: i64,
    pub status: String,
    /// The pair this turn is priced by; see `cost_groups`.
    pub provider: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub usd_micros: Option<i64>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartDetail {
    pub seq: i64,
    pub kind: String,
    pub tool_call_id: String,
    pub signature: String,
    pub data: String,
    pub foreign_id: String,
    /// For a tool result: the tool its call named, or "unknown tool".
    pub tool_name: String,
    pub linked: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageDetail {
    pub seq: i64,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub foreign_id: String,
    pub time_created: i64,
    pub parts: Vec<PartDetail>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionDetail {
    pub session: SessionRow,
    pub turns: Vec<TurnDetail>,
    pub messages: Vec<MessageDetail>,
}

/// One session in full, read in one transaction so it is one moment's view.
pub fn load_session_detail(conn: &Connection, session_id: &str) -> Result<SessionDetail, StoreError> {
    let tx = conn.unchecked_transaction().map_err(|e| other("load session", e))?;
    let mut session = tx
        .query_row(
            "SELECT s.id, s.title, s.model, s.provider, s.harness, s.directory, s.time_created, s.time_updated, w.path, \
             (SELECT b.harness FROM session_binding b WHERE b.session_id = s.id ORDER BY b.id LIMIT 1), \
             (SELECT b.provider FROM session_binding b WHERE b.session_id = s.id ORDER BY b.id LIMIT 1), \
             (SELECT b.foreign_session_id FROM session_binding b WHERE b.session_id = s.id ORDER BY b.id LIMIT 1) \
             FROM session s LEFT JOIN worktree w ON w.id = s.worktree_id WHERE s.id = ?",
            [session_id],
            |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    model: r.get(2)?,
                    provider: r.get(3)?,
                    harness: r.get(4)?,
                    directory: r.get(5)?,
                    time_created: r.get(6)?,
                    time_updated: r.get(7)?,
                    worktree_path: string(r, 8)?,
                    binding_harness: string(r, 9)?,
                    binding_provider: string(r, 10)?,
                    foreign_session_id: string(r, 11)?,
                    ..SessionRow::default()
                })
            },
        )
        .optional()
        .map_err(|e| other("load session", e))?
        .ok_or_else(|| StoreError::NotFound(format!("store: no session {session_id:?}")))?;
    if !session.binding_harness.is_empty() {
        session.harness = session.binding_harness.clone();
    }

    let turns: Vec<TurnDetail> = tx
        .prepare(&format!("SELECT t.seq, t.status, t.cost_input_tokens, t.cost_output_tokens, t.cost_total_tokens, t.cost_cache_read_tokens, t.cost_cache_write_tokens, t.cost_reasoning_tokens, t.cost_usd_micros, t.time_created, t.time_updated, \
             (SELECT m.provider {TURN_MODEL_FROM}), (SELECT m.model {TURN_MODEL_FROM}) FROM turn t WHERE t.session_id = ? ORDER BY t.seq"))
        .and_then(|mut s| {
            s.query_map([session_id], |r| {
                Ok(TurnDetail {
                    seq: r.get(0)?,
                    status: r.get(1)?,
                    input: r.get(2)?,
                    output: r.get(3)?,
                    total: r.get(4)?,
                    cache_read: r.get(5)?,
                    cache_write: r.get(6)?,
                    reasoning: r.get(7)?,
                    usd_micros: r.get(8)?,
                    time_created: r.get(9)?,
                    time_updated: r.get(10)?,
                    provider: string(r, 11)?,
                    model: string(r, 12)?,
                })
            })?
            .collect()
        })
        .map_err(|e| other("load turns", e))?;
    let mut turns = turns;
    for t in &mut turns {
        if t.model.is_empty() {
            (t.provider, t.model) = (session.provider.clone(), session.model.clone());
        }
    }
    session.turn_count = turns.len() as i64;
    for t in &turns {
        session.sum_input += t.input;
        session.sum_output += t.output;
        session.sum_total += t.total;
        session.sum_cache_read += t.cache_read;
        session.sum_cache_write += t.cache_write;
        session.sum_reasoning += t.reasoning;
    }

    let messages: Vec<(String, MessageDetail)> = tx
        .prepare("SELECT id, seq, role, provider, model, foreign_id, time_created FROM message WHERE session_id = ? ORDER BY seq")
        .and_then(|mut s| {
            s.query_map([session_id], |r| {
                Ok((
                    r.get(0)?,
                    MessageDetail {
                        seq: r.get(1)?,
                        role: r.get(2)?,
                        provider: r.get(3)?,
                        model: r.get(4)?,
                        foreign_id: string(r, 5)?,
                        time_created: r.get(6)?,
                        parts: Vec::new(),
                    },
                ))
            })?
            .collect()
        })
        .map_err(|e| other("load messages", e))?;

    let parts: Vec<(String, PartDetail)> = tx
        .prepare("SELECT message_id, seq, type, tool_call_id, signature, data, foreign_id FROM part WHERE session_id = ? ORDER BY message_id, seq")
        .and_then(|mut s| {
            s.query_map([session_id], |r| {
                Ok((
                    r.get(0)?,
                    PartDetail {
                        seq: r.get(1)?,
                        kind: r.get(2)?,
                        tool_call_id: string(r, 3)?,
                        signature: string(r, 4)?,
                        data: r.get(5)?,
                        foreign_id: string(r, 6)?,
                        ..PartDetail::default()
                    },
                ))
            })?
            .collect()
        })
        .map_err(|e| other("load parts", e))?;
    tx.commit().map_err(|e| other("load session", e))?;

    // A tool result names the tool its call used, first call per id winning.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for (_, p) in &parts {
        if p.kind == "tool_call" && !p.tool_call_id.is_empty() {
            let name = tool_name_of(&p.data);
            if !name.is_empty() {
                tool_names.entry(p.tool_call_id.clone()).or_insert(name);
            }
        }
    }
    let mut by_message: HashMap<String, Vec<PartDetail>> = HashMap::new();
    for (message_id, mut p) in parts {
        if p.kind == "tool_result" {
            match tool_names.get(&p.tool_call_id).filter(|_| !p.tool_call_id.is_empty()) {
                Some(name) => (p.tool_name, p.linked) = (name.clone(), true),
                None => (p.tool_name, p.linked) = ("unknown tool".into(), false),
            }
        }
        by_message.entry(message_id).or_default().push(p);
    }
    let messages = messages
        .into_iter()
        .map(|(id, mut m)| {
            m.parts = by_message.remove(&id).unwrap_or_default();
            m
        })
        .collect();
    Ok(SessionDetail { session, turns, messages })
}

/// Every session spawned under this one — subagents, and theirs. Their spend is part of what the parent spent.
pub fn descendant_session_ids(conn: &Connection, session_id: &str) -> Result<Vec<String>, StoreError> {
    conn.prepare(
        // UNION on the id alone: each session once, and a parent loop in
        // the data ends instead of recursing.
        "WITH RECURSIVE tree(id) AS (SELECT id FROM session WHERE parent_id = ?1 \
         UNION SELECT s.id FROM session s JOIN tree t ON s.parent_id = t.id) \
         SELECT id FROM tree WHERE id != ?1 ORDER BY id",
    )
    .and_then(|mut st| st.query_map([session_id], |r| r.get(0))?.collect())
    .map_err(|e| other("list child sessions", e))
}

/// The `name` a tool call's data carries.
pub fn tool_name_of(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data).ok().and_then(|v| v.get("name")?.as_str().map(String::from)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Home;
    use crate::*;

    fn thread(foreign: &str, provider: &str) -> Thread {
        Thread {
            worktree: Worktree { path: "/repo".into(), ..Worktree::default() },
            session: Session { title: format!("t {foreign}"), harness: "claude".into(), ..Session::default() },
            binding: Binding { provider: provider.into(), harness: "claude".into(), foreign_session_id: foreign.into(), ..Binding::default() },
            turns: vec![Turn { cost_input: 2, ..Turn::default() }, Turn { cost_input: 3, ..Turn::default() }],
            messages: vec![Message {
                role: Role::Assistant,
                provider: provider.into(),
                model: "m".into(),
                foreign_id: "x".into(),
                usage: String::new(),
                raw_json: None,
                turn_seq: None,
                parts: vec![
                    Part { kind: "tool_call".into(), tool_call_id: "c1".into(), data: r#"{"name":"Bash"}"#.into(), ..Part::default() },
                    Part { kind: "tool_result".into(), tool_call_id: "c1".into(), ..Part::default() },
                    Part { kind: "tool_result".into(), tool_call_id: "c9".into(), ..Part::default() },
                ],
            }],
            ..Thread::default()
        }
    }

    #[test]
    fn a_session_lists_resolves_and_reads_back_in_full() {
        let home = Home::new("query");
        let env = home.env();
        let conn = open(&env).unwrap();
        Writer::new(&conn).ingest(&thread("s1", "anthropic")).unwrap();
        Writer::new(&conn).ingest(&thread("s2", "anthropic")).unwrap();
        let rows = list_sessions(&conn, "", "", 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].turn_count, rows[0].sum_input), (2, 5));
        assert!(list_sessions(&conn, "cursor", "", 0).unwrap().is_empty());
        assert_eq!(list_sessions(&conn, "", "/repo", 1).unwrap().len(), 1);
        assert!(list_sessions(&conn, "", "", -2).is_err());

        let id = resolve_session_id(&conn, "s1").unwrap();
        assert_eq!(resolve_session_id(&conn, &id).unwrap(), id);
        assert_eq!(resolve_session_id(&conn, &id[..id.len() - 1]).unwrap(), id, "a unique prefix resolves");
        assert!(matches!(resolve_session_id(&conn, "short"), Err(StoreError::NotFound(_))));
        let d = load_session_detail(&conn, &id).unwrap();
        let parts = &d.messages[0].parts;
        assert_eq!((parts[1].tool_name.as_str(), parts[1].linked), ("Bash", true));
        assert_eq!((parts[2].tool_name.as_str(), parts[2].linked), ("unknown tool", false));
        assert!(matches!(load_session_detail(&conn, "nope"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn one_foreign_id_under_two_providers_is_ambiguous_by_name() {
        let home = Home::new("ambiguous");
        let env = home.env();
        let conn = open(&env).unwrap();
        Writer::new(&conn).ingest(&thread("same", "anthropic")).unwrap();
        Writer::new(&conn).ingest(&thread("same", "openai")).unwrap();
        match resolve_session_id(&conn, "same") {
            Err(StoreError::Ambiguous { ids, .. }) => assert_eq!(ids.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_turn_is_priced_by_its_own_model_and_synthetic_names_no_model() {
        let home = Home::new("groups");
        let env = home.env();
        let conn = open(&env).unwrap();
        let msg = |model: &str, turn: i64, fid: &str| Message {
            role: Role::Assistant,
            provider: "anthropic".into(),
            model: model.into(),
            foreign_id: fid.into(),
            usage: String::new(),
            raw_json: None,
            turn_seq: Some(turn),
            parts: Vec::new(),
        };
        let th = Thread {
            worktree: Worktree { path: "/repo".into(), ..Worktree::default() },
            session: Session { title: "hop".into(), harness: "claude".into(), provider: "anthropic".into(), model: "fable".into(), ..Session::default() },
            binding: Binding { provider: "claude".into(), harness: "claude".into(), foreign_session_id: "s".into(), ..Binding::default() },
            turns: vec![Turn { cost_input: 1, ..Turn::default() }, Turn { cost_input: 2, ..Turn::default() }, Turn { cost_input: 4, cost_usd_micros: Some(9), ..Turn::default() }],
            messages: vec![msg("opus", 0, "a"), msg("sonnet", 1, "b"), msg("<synthetic>", 1, "c")],
            ..Thread::default()
        };
        Writer::new(&conn).ingest(&th).unwrap();
        let id = resolve_session_id(&conn, "s").unwrap();
        let groups = &cost_groups(&conn, std::slice::from_ref(&id)).unwrap()[&id];
        let got: Vec<(&str, bool, i64, i64)> = groups.iter().map(|g| (g.model.as_str(), g.reported, g.input, g.usd_micros)).collect();
        assert_eq!(got, vec![("fable", true, 4, 9), ("opus", false, 1, 0), ("sonnet", false, 2, 0)], "no message: the session's model");
        assert_eq!(cost_groups(&conn, &[]).unwrap()[&id], *groups, "no ids is every session");
        let d = load_session_detail(&conn, &id).unwrap();
        assert_eq!(d.turns.iter().map(|t| t.model.as_str()).collect::<Vec<_>>(), vec!["opus", "sonnet", "fable"]);

        // A store from before the link: a re-import links what it skips.
        conn.execute("UPDATE message SET turn_id = NULL", []).unwrap();
        assert_eq!(load_session_detail(&conn, &id).unwrap().turns[0].model, "fable");
        Writer::new(&conn).ingest(&th).unwrap();
        assert_eq!(load_session_detail(&conn, &id).unwrap().turns[0].model, "opus");
    }
}
