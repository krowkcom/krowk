//! Reconciling a provider's usage ledger against the transcripts on disk.
//!
//! A ledger row is one metered execution as the provider billed it. Most of
//! them are also in a local transcript; the ones that are not are zombie
//! executions — a request the client killed or timed out on that still ran,
//! and billed, on the provider's side. Only the ledger knows about those.
//!
//! Ledger sessions are imported as messages only (harness `ledger`, one
//! message per row, deduped on the provider's id). Reconciling owns their
//! turns: one per message, numbered by the message's own seq — which never
//! changes once stored, however the file is re-exported — and linked to it.
//! It then decides, per row, whether anything else already accounts for it:
//!
//! - **duplicate**: an earlier-stored ledger message has the same provider
//!   and id — the same execution in two exports. Zeroed, counted once.
//! - **observed**: an assistant message outside the ledger has the same model
//!   and the same input and completion token counts. The turn keeps its
//!   message (the provider's record) but its token columns are zeroed, so the
//!   execution is counted once — from the transcript.
//! - **unobserved**: nothing local matches. The turn carries the row's tokens
//!   and any cost the provider reported, and every roll-up counts it.
//!
//! Each local message accounts for at most one ledger row, so two identical
//! calls where the client saw one leave one unobserved. Claude writes one
//! transcript line per content block, each repeating its API message's
//! usage — with output growing as it streams — so Claude lines sharing a
//! `message.id` are one message at its largest output. A Claude call killed
//! mid-stream leaves only partial counts, so its ledger row stays unobserved
//! even though the transcript counted its input: the provider's figure is
//! the one it billed. Rows are taken in
//! the order they were first stored, which makes the answer the same on
//! every run. Reconciling reads
//! only what is stored — the message usage is the source of truth, the turn
//! columns are derived from it — so running it again changes nothing, and a
//! transcript imported after the ledger turns its row observed on the next run.

use crate::{other, StoreError};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashMap;

/// The harness every ledger session is stored under.
pub const LEDGER_HARNESS: &str = "ledger";
pub const STATUS_OBSERVED: &str = "observed";
pub const STATUS_UNOBSERVED: &str = "unobserved";
pub const STATUS_DUPLICATE: &str = "duplicate";

/// What one reconciliation found across every ledger session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub observed: usize,
    pub unobserved: usize,
    pub duplicate: usize,
}

/// Input and completion tokens as a transcript's usage block reports them,
/// whichever of the shapes the importers store: Anthropic's
/// `input_tokens`/`output_tokens` (thinking inside output), opencode's
/// `input`/`output` with `reasoning` beside output, OpenAI's
/// `prompt_tokens`/`completion_tokens`.
const FINGERPRINT: &str = "COALESCE(json_extract(m.usage, '$.input_tokens'), json_extract(m.usage, '$.input'), json_extract(m.usage, '$.prompt_tokens')) AS input, \
     COALESCE(json_extract(m.usage, '$.output_tokens'), json_extract(m.usage, '$.output') + COALESCE(json_extract(m.usage, '$.reasoning'), 0), json_extract(m.usage, '$.completion_tokens')) AS output";

/// Gives every ledger message its turn and marks each turn duplicate,
/// observed or unobserved, in one transaction.
pub fn reconcile_ledger(conn: &Connection) -> Result<Reconciled, StoreError> {
    let tx = conn.unchecked_transaction().map_err(|e| other("reconcile ledger", e))?;
    let now = crate::now_ms();
    // One turn per message, at the message's seq, created once.
    let missing: Vec<(String, i64)> = tx
        .prepare(
            "SELECT m.session_id, m.seq FROM message m JOIN session s ON s.id = m.session_id \
             WHERE s.harness = ?1 AND NOT EXISTS (SELECT 1 FROM turn t WHERE t.session_id = m.session_id AND t.seq = m.seq)",
        )
        .and_then(|mut st| st.query_map([LEDGER_HARNESS], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .map_err(|e| other("reconcile ledger: find new rows", e))?;
    for (session_id, seq) in missing {
        tx.execute(
            "INSERT INTO turn (id, session_id, seq, status, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?)",
            params![crate::new_id(), session_id, seq, STATUS_UNOBSERVED, now, now],
        )
        .map_err(|e| other("reconcile ledger: insert turn", e))?;
    }
    tx.execute(
        "UPDATE message SET turn_id = (SELECT t.id FROM turn t WHERE t.session_id = message.session_id AND t.seq = message.seq) \
         WHERE turn_id IS NULL AND session_id IN (SELECT id FROM session WHERE harness = ?1)",
        [LEDGER_HARNESS],
    )
    .map_err(|e| other("reconcile ledger: link turns", e))?;

    // Oldest-stored first, so the first export of an execution keeps it.
    let rows: Vec<(String, String, String, String, String)> = tx
        .prepare(
            "SELECT t.id, m.provider, COALESCE(m.foreign_id, ''), m.model, m.usage FROM message m JOIN turn t ON t.id = m.turn_id \
             JOIN session s ON s.id = m.session_id WHERE s.harness = ?1 ORDER BY m.id",
        )
        .and_then(|mut st| st.query_map([LEDGER_HARNESS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect())
        .map_err(|e| other("reconcile ledger: read ledger rows", e))?;
    if rows.is_empty() {
        tx.commit().map_err(|e| other("reconcile ledger", e))?;
        return Ok(Reconciled::default());
    }
    let mut local: HashMap<(String, i64, i64), usize> = HashMap::new();
    {
        let mut st = tx
            .prepare(&format!(
                "SELECT model, input, MAX(output) FROM (SELECT m.model AS model, {FINGERPRINT}, \
                 CASE WHEN s.harness = 'claude' AND json_valid(m.raw_json) THEN json_extract(m.raw_json, '$.message.id') END AS call, m.id AS mid \
                 FROM message m JOIN session s ON s.id = m.session_id \
                 WHERE s.harness != ?1 AND m.role = 'assistant' \
                 AND m.model IN (SELECT DISTINCT lm.model FROM message lm JOIN session ls ON ls.id = lm.session_id WHERE ls.harness = ?1)) \
                 GROUP BY model, COALESCE(call, mid)"
            ))
            .map_err(|e| other("reconcile ledger: read transcripts", e))?;
        let found = st
            .query_map([LEDGER_HARNESS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)))
            .map_err(|e| other("reconcile ledger: read transcripts", e))?;
        for f in found {
            if let (model, Some(input), Some(output)) = f.map_err(|e| other("reconcile ledger: read transcripts", e))? {
                *local.entry((model, input, output)).or_default() += 1;
            }
        }
    }
    let zero = |status: &str, turn_id: &str| {
        tx.execute(
            "UPDATE turn SET status = ?, cost_input_tokens = 0, cost_output_tokens = 0, cost_total_tokens = 0, cost_cache_read_tokens = 0, cost_cache_write_tokens = 0, cost_reasoning_tokens = 0, cost_usd_micros = NULL, time_updated = ? WHERE id = ?",
            params![status, now, turn_id],
        )
    };
    let mut out = Reconciled::default();
    let mut executions = std::collections::HashSet::new();
    for (turn_id, provider, foreign_id, model, usage) in rows {
        let u = LedgerUsage::parse(&usage);
        let result = if !executions.insert((provider, foreign_id)) {
            out.duplicate += 1;
            zero(STATUS_DUPLICATE, &turn_id)
        } else if let Some(n) = local.get_mut(&(model, u.input, u.output)).filter(|n| **n > 0) {
            *n -= 1;
            out.observed += 1;
            zero(STATUS_OBSERVED, &turn_id)
        } else {
            out.unobserved += 1;
            let t = u.turn_columns();
            tx.execute(
                "UPDATE turn SET status = ?, cost_input_tokens = ?, cost_output_tokens = ?, cost_total_tokens = ?, cost_cache_read_tokens = ?, cost_cache_write_tokens = ?, cost_reasoning_tokens = ?, cost_usd_micros = ?, time_updated = ? WHERE id = ?",
                params![STATUS_UNOBSERVED, t.cost_input, t.cost_output, t.cost_total, t.cost_cache_read, t.cost_cache_write, t.cost_reasoning, t.cost_usd_micros, now, turn_id],
            )
        };
        result.map_err(|e| other("reconcile ledger: update turn", e))?;
    }
    tx.commit().map_err(|e| other("reconcile ledger", e))?;
    Ok(out)
}

/// One ledger row's usage, as the ledger importer stores it on the message:
/// `output_tokens` is every completion token billed, `reasoning_tokens` the
/// share of those that was reasoning.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LedgerUsage {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: Option<f64>,
}

impl LedgerUsage {
    pub fn parse(usage: &str) -> LedgerUsage {
        let v: Value = serde_json::from_str(usage).unwrap_or(Value::Null);
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
        LedgerUsage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            reasoning: n("reasoning_tokens"),
            cache_read: n("cache_read_tokens"),
            cache_write: n("cache_write_tokens"),
            cost_usd: v.get("cost_usd").and_then(Value::as_f64).filter(|c| c.is_finite() && *c >= 0.0),
        }
    }

    /// The turn columns for an unobserved row. Reasoning is split out of
    /// output, as every other importer stores it, so a model with its own
    /// reasoning rate prices it without a re-import.
    pub fn turn_columns(&self) -> crate::Turn {
        let reasoning = self.reasoning.min(self.output);
        crate::Turn {
            status: STATUS_UNOBSERVED.into(),
            cost_input: self.input,
            cost_output: self.output - reasoning,
            cost_reasoning: reasoning,
            cost_cache_read: self.cache_read,
            cost_cache_write: self.cache_write,
            cost_total: self.input + self.output + self.cache_read + self.cache_write,
            cost_usd_micros: self.cost_usd.map(|c| (c * 1e6).round() as i64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Home;
    use crate::*;

    fn usage(input: i64, output: i64, reasoning: i64, cost: Option<f64>) -> String {
        serde_json::json!({ "input_tokens": input, "output_tokens": output, "reasoning_tokens": reasoning, "cost_usd": cost }).to_string()
    }

    fn ledger(file: &str, rows: &[(&str, &str, String)]) -> Thread {
        let messages = rows
            .iter()
            .map(|(id, model, u)| Message {
                role: Role::Assistant,
                provider: "opencode".into(),
                model: (*model).into(),
                foreign_id: (*id).into(),
                usage: u.clone(),
                raw_json: None,
                turn_seq: None,
                parts: Vec::new(),
            })
            .collect();
        Thread {
            worktree: Worktree { path: "/ledger".into(), vcs: "none".into(), ..Worktree::default() },
            session: Session { title: "ledger".into(), harness: LEDGER_HARNESS.into(), provider: "opencode".into(), ..Session::default() },
            binding: Binding { provider: LEDGER_HARNESS.into(), harness: LEDGER_HARNESS.into(), foreign_session_id: file.into(), ..Binding::default() },
            messages,
            ..Thread::default()
        }
    }

    fn transcript(model: &str, usage: &str) -> Thread {
        Thread {
            worktree: Worktree { path: "/repo".into(), ..Worktree::default() },
            session: Session { title: "local".into(), harness: "opencode".into(), provider: "opencode".into(), ..Session::default() },
            binding: Binding { provider: "opencode".into(), harness: "opencode".into(), foreign_session_id: "ses_1".into(), ..Binding::default() },
            turns: vec![Turn::default()],
            messages: vec![Message {
                role: Role::Assistant,
                provider: "opencode".into(),
                model: model.into(),
                foreign_id: "msg_1".into(),
                usage: usage.into(),
                raw_json: None,
                turn_seq: None,
                parts: Vec::new(),
            }],
            ..Thread::default()
        }
    }

    fn statuses(conn: &Connection) -> Vec<(String, i64, i64, Option<i64>)> {
        conn.prepare("SELECT t.status, t.cost_output_tokens, t.cost_reasoning_tokens, t.cost_usd_micros FROM turn t JOIN session s ON s.id = t.session_id WHERE s.harness = 'ledger' ORDER BY t.seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_row_a_transcript_saw_is_counted_once_and_a_ghost_row_is_counted() {
        let home = Home::new("ledger");
        let env = home.env();
        let conn = open(&env).unwrap();
        let rows = [
            ("gen_seen", "qwen3.5-plus", usage(84, 1379, 1362, None)),
            ("gen_ghost", "qwen3.5-plus", usage(84, 3422, 3405, Some(0.0041))),
        ];
        let w = Writer::new(&conn);
        w.ingest(&ledger("zen", &rows)).unwrap();
        // opencode keeps reasoning beside output: 17 + 1362 is the 1379 billed.
        w.ingest(&transcript("qwen3.5-plus", r#"{"input":84,"output":17,"reasoning":1362,"cache":{"read":0,"write":0}}"#)).unwrap();

        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 1, unobserved: 1, duplicate: 0 });
        assert_eq!(
            statuses(&conn),
            vec![("observed".into(), 0, 0, None), ("unobserved".into(), 17, 3405, Some(4100))],
            "the ghost keeps its reasoning split and the provider's cost"
        );
        // Converges: a re-import and a second pass change nothing.
        w.ingest(&ledger("zen", &rows)).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 1, unobserved: 1, duplicate: 0 });
        assert_eq!(statuses(&conn)[1].0, "unobserved");
    }

    #[test]
    fn one_transcript_message_accounts_for_one_row_and_a_late_transcript_flips_it() {
        let home = Home::new("ledger-late");
        let env = home.env();
        let conn = open(&env).unwrap();
        let w = Writer::new(&conn);
        let rows = [("a", "m", usage(10, 20, 0, None)), ("b", "m", usage(10, 20, 0, None))];
        w.ingest(&ledger("zen", &rows)).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 0, unobserved: 2, duplicate: 0 });
        w.ingest(&transcript("m", r#"{"input_tokens":10,"output_tokens":20}"#)).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 1, unobserved: 1, duplicate: 0 });
        let st = statuses(&conn);
        assert_eq!((st[0].0.as_str(), st[1].0.as_str()), ("observed", "unobserved"));
        assert_eq!(reconcile_ledger(&Connection::open_in_memory().unwrap()).map_err(|_| ()), Err(()), "no schema, no answer");
    }

    #[test]
    fn a_reordered_re_export_keeps_every_row_on_its_own_turn_and_a_second_export_counts_once() {
        let home = Home::new("ledger-reorder");
        let env = home.env();
        let conn = open(&env).unwrap();
        let w = Writer::new(&conn);
        let (a, b, c) = (("a", "m", usage(1, 100, 0, Some(1.0))), ("b", "m", usage(2, 200, 0, Some(2.0))), ("c", "m", usage(3, 300, 0, Some(3.0))));
        w.ingest(&ledger("zen", &[a.clone(), b.clone()])).unwrap();
        reconcile_ledger(&conn).unwrap();
        // A row prepended and the file truncated to it and a: nothing moves.
        w.ingest(&ledger("zen", &[c.clone(), a.clone(), b.clone()])).unwrap();
        w.ingest(&ledger("zen", &[c.clone(), a.clone()])).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 0, unobserved: 3, duplicate: 0 });
        let costs: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT m.foreign_id, t.cost_usd_micros FROM message m JOIN turn t ON t.id = m.turn_id ORDER BY m.seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(costs, vec![("a".into(), Some(1_000_000)), ("b".into(), Some(2_000_000)), ("c".into(), Some(3_000_000))]);
        // The same executions in a second file are the same executions.
        w.ingest(&ledger("zen-copy", &[a, c])).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 0, unobserved: 3, duplicate: 2 });
    }

    #[test]
    fn claude_lines_repeating_one_api_message_match_one_row() {
        let home = Home::new("ledger-claude");
        let env = home.env();
        let conn = open(&env).unwrap();
        let w = Writer::new(&conn);
        let rows = [("a", "m", usage(10, 20, 0, None)), ("b", "m", usage(10, 20, 0, None))];
        w.ingest(&ledger("zen", &rows)).unwrap();
        let mut th = transcript("m", r#"{"input_tokens":10,"output_tokens":20}"#);
        let line = th.messages[0].clone();
        // Streaming lines of one call: output grows, the last is the billed count.
        th.session.harness = "claude".into();
        th.messages = [("l1", 1), ("l2", 20), ("l3", 20)]
            .iter()
            .map(|(id, out)| Message {
                foreign_id: (*id).into(),
                usage: format!(r#"{{"input_tokens":10,"output_tokens":{out}}}"#),
                raw_json: Some(r#"{"message":{"id":"msg_1"}}"#.into()),
                ..line.clone()
            })
            .collect();
        w.ingest(&th).unwrap();
        assert_eq!(reconcile_ledger(&conn).unwrap(), Reconciled { observed: 1, unobserved: 1, duplicate: 0 });
    }
}
