//! `krowk sessions` and `krowk pricing refresh`: the device-side commands,
//! reading and writing krowk.db on this machine. Nothing here touches the
//! registry; `sessions sync` is the one command allowed network, and only to
//! refresh prices.

use super::{interactive, Ctx};
use crate::output::{self, Envelope, Format};
use crate::pricing;
use crate::termclean;
use krowk_api::{fail, Error};
use krowk_import::{Ref, Source};
use krowk_store::{Connection, CostGroup, SessionDetail, SessionRow, StoreError, TurnDetail};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub(super) const DEFAULT_SESSION_LIMIT: usize = 50;
const MAX_REPORTED_ERRORS: usize = 10;
const MAX_SKIPPED_TYPES: usize = 32;
const MAX_SKIPPED_TYPE_LEN: usize = 64;
const MAX_ERROR_REASON_LEN: usize = 512;
const SKIPPED_TYPE_OTHER: &str = "krowk:other";
const UNSUPPORTED_OS: &str = "sessions is not supported on Windows in v1";
const PRICING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Every source import, sync and rebuild read: the transcripts on this
/// machine, and in the harness build krowk's own session logs too.
fn all_sources() -> Vec<Box<dyn Source>> {
    #[allow(unused_mut)]
    let mut all = krowk_import::sources();
    #[cfg(feature = "harness")]
    all.push(Box::new(krowk_harness::project::Krowk));
    all
}

/// Brings one native session's rows up to date from its log, right after a
/// `krowk -p` turn: the same read and write an import does, for its log and
/// for every subagent's under it (R-SUB-6), parents first, so each child
/// links to the session that started it.
#[cfg(feature = "harness")]
pub(super) fn project_native(env: krowk_import::Env, session_id: &str) -> Result<(), Error> {
    let store_path = krowk_store::db_path(env).map_err(|e| store_fail(&e, ""))?.display().to_string();
    let src = krowk_harness::project::Krowk;
    let found = src.discover(env).map_err(|e| fail("import_failed", e.to_string()))?;
    let Some(r) = found.iter().find(|r| r.id == session_id) else {
        return Err(fail("import_failed", format!("the log of session {session_id} is not where krowk keeps sessions")));
    };
    let mut tree = vec![r.clone()];
    if let Ok(dir) = krowk_harness::log::sessions_dir(env) {
        let children = krowk_harness::budget::descendants(&dir, session_id);
        tree.extend(children.iter().filter_map(|(id, _)| found.iter().find(|r| &r.id == id).cloned()));
    }
    let _lock = lock_store_waiting(&store_path, REFRESH_LOCK_WAIT)?;
    let conn = krowk_store::open(env).map_err(|e| store_fail(&e, &store_path))?;
    for r in &tree {
        let (thread, next, _) = src.read(env, r, "").map_err(|e| fail("import_failed", e.to_string()))?;
        krowk_store::Writer::new(&conn).ingest_with_cursor(&thread, &r.key(), &next).map_err(|e| store_fail(&e, &store_path))?;
    }
    Ok(())
}

/// Brings an archived native session back into its directory and into
/// krowk.db (R-VINT-4): fetched from its week's vintage, opened, written
/// back, then projected as a `krowk -p` turn's log is, the archive's
/// stand-in rows dropped first.
#[cfg(all(feature = "harness", unix))]
pub(super) fn restore_native(ctx: &Ctx, session_id: &str) -> Result<(), Error> {
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    let (client, account) = super::sync::vintage_keys(ctx)?;
    krowk_harness::vintage::restore(&client, &account, &dir, session_id).map_err(|e| fail("restore_failed", e))?;
    let conn = open_store(ctx)?;
    krowk_store::drop_bodies(&conn, krowk_harness::project::HARNESS, session_id).map_err(|e| store_fail(&e, &db_path_string(ctx)))?;
    drop(conn);
    project_native(ctx.io.env, session_id)
}

/// Restores a native session named by its log id when it is archived; a
/// no-op for any other.
#[cfg(all(feature = "harness", unix))]
pub(super) fn restore_if_archived(ctx: &Ctx, session_id: &str) -> Result<bool, Error> {
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    if !krowk_harness::vintage::is_archived(&dir, session_id) {
        return Ok(false);
    }
    restore_native(ctx, session_id)?;
    Ok(true)
}

/// The idle cut-off `krowk sessions archive` uses: `--older-than`, else
/// KROWK_ARCHIVE_AFTER_DAYS, else 14 days.
#[cfg(all(feature = "harness", unix))]
fn idle_days(ctx: &Ctx) -> Result<u64, Error> {
    let (given, from) = match ctx.f.older_than.trim() {
        "" => (ctx.env("KROWK_ARCHIVE_AFTER_DAYS"), "KROWK_ARCHIVE_AFTER_DAYS"),
        v => (v.to_string(), "--older-than"),
    };
    if given.trim().is_empty() {
        return Ok(krowk_harness::vintage::DEFAULT_IDLE_DAYS);
    }
    given.trim().parse().map_err(|_| fail("bad_flag", format!("{from} is a whole number of days, got {:?}", given.trim())))
}

/// `krowk sessions archive`: every native session idle past the cut-off
/// leaves the machine for its week's vintage (R-VINT-1, R-VINT-2), and
/// keeps its index row in krowk.db (R-VINT-3). `--weekly` is the weekly
/// job: it does nothing until a week has passed since the last run.
#[cfg(all(feature = "harness", unix))]
pub fn archive(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    check_os()?;
    if !args.is_empty() {
        return Err(fail("bad_flag", format!("`krowk sessions archive` takes no arguments, got {}", args.join(" "))));
    }
    let days = idle_days(ctx)?;
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    let now = now_ms();
    if ctx.f.weekly && !krowk_harness::vintage::due(&dir, now) {
        return emit_data(ctx, json!({ "archived": [], "due": false }), "not due: the last archive ran less than a week ago".into());
    }
    let (client, account) = super::sync::vintage_keys(ctx)?;
    let run = krowk_harness::vintage::archive(&client, &account, &dir, now, days);
    // The weeks stored before a failure are archived: their bodies leave
    // krowk.db whether or not the rest of the run went through.
    let done = run.archived;
    let conn = open_store(ctx)?;
    for a in &done {
        krowk_store::drop_bodies(&conn, krowk_harness::project::HARNESS, &a.id).map_err(|e| store_fail(&e, &db_path_string(ctx)))?;
    }
    drop(conn);
    for a in &done {
        project_native(ctx.io.env, &a.id)?;
    }
    if let Some(e) = run.failed {
        return Err(fail("archive_failed", if done.is_empty() { e } else { format!("{e} ({} session(s) archived before it)", done.len()) }));
    }
    let rows: Vec<Value> = done.iter().map(|a| json!({ "id": a.id, "week": a.week })).collect();
    let n = rows.len();
    emit_data(ctx, json!({ "archived": rows, "due": true }), format!("{n} session{} archived", if n == 1 { "" } else { "s" }))
}

/// `krowk sessions restore <id>`: an archived session back on the machine.
#[cfg(all(feature = "harness", unix))]
pub fn restore(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    check_os()?;
    let id = native_arg(ctx, args, "restore")?;
    if !restore_if_archived(ctx, &id)? {
        return Err(fail("not_archived", format!("session {id} is not archived on this machine")));
    }
    emit_data(ctx, json!({ "restored": id }), format!("session {id} restored"))
}

/// `krowk sessions pin|unpin <id>`: a pinned session is never archived.
#[cfg(all(feature = "harness", unix))]
pub fn pin(ctx: &mut Ctx, args: &[String], pinned: bool) -> Result<(), Error> {
    check_os()?;
    let verb = if pinned { "pin" } else { "unpin" };
    let id = native_arg(ctx, args, verb)?;
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    krowk_harness::vintage::pin(&dir, &id, pinned).map_err(|e| fail("no_session", e))?;
    emit_data(ctx, json!({ "id": id, "pinned": pinned }), format!("session {id} {verb}ned"))
}

/// The native session a `krowk sessions <verb>` argument names, by its log
/// id: the id itself when its directory is here, else krowk.db's answer.
#[cfg(all(feature = "harness", unix))]
fn native_arg(ctx: &Ctx, args: &[String], verb: &str) -> Result<String, Error> {
    let dir = krowk_harness::log::sessions_dir(ctx.io.env)?;
    if let [a] = args
        && krowk_harness::log::valid_id(a.trim())
        && dir.join(a.trim()).is_dir()
    {
        return Ok(a.trim().to_string());
    }
    let conn = open_store(ctx)?;
    let id = resolve_arg(ctx, &conn, args, verb)?;
    let d = load_by_id(ctx, &conn, &id)?;
    if d.session.harness != krowk_harness::project::HARNESS {
        return Err(fail("no_session", format!("{id} is a {} session — only krowk's own sessions are archived", d.session.harness)));
    }
    Ok(d.session.foreign_session_id)
}

pub(super) fn check_os() -> Result<(), Error> {
    krowk_import::check_os().map_err(|_| fail("unsupported_os", UNSUPPORTED_OS))
}

/// What a store failure says to a person. SQLite's "database is locked"
/// names no action, so it is replaced by who is at fault.
fn sanitize_store_err(msg: &str, store_path: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("database is locked") || lower.contains("database table is locked") || lower.contains("database is busy") {
        return format!("the store at {store_path} is busy — another process is writing to it");
    }
    msg.to_string()
}

pub(super) fn store_fail(e: &StoreError, store_path: &str) -> Error {
    fail("store_unavailable", sanitize_store_err(e.message(), store_path))
}

pub(super) fn db_path_string(ctx: &Ctx) -> String {
    krowk_store::db_path(ctx.io.env).map(|p| p.display().to_string()).unwrap_or_default()
}

pub(super) fn open_store(ctx: &Ctx) -> Result<Connection, Error> {
    krowk_store::open(ctx.io.env).map_err(|e| store_fail(&e, &db_path_string(ctx)))
}

/// Where krowk.db lives, or store.open's own sentence for an environment
/// that names no home.
fn resolve_store_path(ctx: &Ctx) -> Result<String, Error> {
    let path = db_path_string(ctx);
    if !path.is_empty() {
        return Ok(path);
    }
    match krowk_store::open(ctx.io.env) {
        Err(e) => Err(store_fail(&e, "")),
        Ok(_) => Err(fail("store_unavailable", "store: no home directory in environment")),
    }
}

// ---- list -------------------------------------------------------------------

pub fn list(ctx: &mut Ctx) -> Result<(), Error> {
    check_os()?;
    if ctx.f.limit < 0 {
        return Err(fail("bad_flag", "--limit is a maximum, so it cannot be negative"));
    }
    let limit = if ctx.f.all { -1 } else { ctx.f.limit };
    let conn = open_store(ctx)?;
    let rows = krowk_store::list_sessions(&conn, &ctx.f.harness, &ctx.f.worktree, limit).map_err(|e| store_fail(&e, &db_path_string(ctx)))?;
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let groups = if ids.is_empty() { HashMap::new() } else { krowk_store::cost_groups(&conn, &ids).map_err(|e| store_fail(&e, &db_path_string(ctx)))? };
    let priced: Vec<Priced> = rows.iter().map(|r| price_session(ctx, r, groups.get(&r.id))).collect();
    let bases: BTreeSet<pricing::Basis> = priced.iter().flat_map(|p| p.bases.iter().copied()).collect();
    let now = now_ms();
    if ctx.format != Format::Human {
        let sessions: Vec<Value> = rows.iter().zip(&priced).map(|(r, p)| session_row_json(r, p, now)).collect();
        let mut data = json!({ "sessions": sessions });
        if !bases.is_empty() {
            data["priced_with"] = json!(pricing::basis_note(&bases));
        }
        return emit_data(ctx, data, format!("{} sessions", rows.len()));
    }
    // One option per row: an unbounded page would hang the terminal building
    // them, so the picker only fires on a paged list.
    if interactive(ctx) && !ctx.f.all && !rows.is_empty() && rows.len() <= DEFAULT_SESSION_LIMIT {
        let id = pick_session(&rows, now)?;
        let _ = writeln!(ctx.io.stdout, "krowk sessions show {id}");
        return Ok(());
    }
    let table = human_sessions_list(&rows, &priced, ctx.colour, now);
    let _ = write!(ctx.io.stdout, "{table}");
    if !rows.is_empty() {
        let _ = writeln!(ctx.io.stdout);
        let note = cost_footnote(&bases, priced.iter().any(|p| p.total().is_none()));
        if !note.is_empty() {
            let _ = writeln!(ctx.io.stdout, "{}", paint(ctx.colour, "2", &note));
        }
    }
    Ok(())
}

pub(super) fn emit_data(ctx: &mut Ctx, data: Value, summary: String) -> Result<(), Error> {
    let rendered = if ctx.f.quiet {
        output::encode(&data)
    } else {
        output::encode(&Envelope { ok: true, data: Some(data), summary, ..Envelope::default() })
    };
    ctx.emit(&rendered)
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A session's cost: the dollars that could be priced or were reported,
/// every (provider, model) with neither, and the price sources used. A cost
/// with an unpriced pair in it is unknown — shown as —, never as the part
/// that could be priced, and never as a silent 0.
///
/// Turns with no tokens and no price (a turn the model never answered, a
/// Claude `<synthetic>` error) cost nothing wherever something else in the
/// session is priced; only when nothing is — a harness that records no
/// usage at all — do they make the cost unknown.
#[derive(Debug, Default)]
pub(super) struct Priced {
    usd: f64,
    unpriced: BTreeSet<String>,
    unpriced_empty: BTreeSet<String>,
    known: bool,
    /// Every turn is counted in another session.
    elsewhere: bool,
    pub(super) bases: BTreeSet<pricing::Basis>,
    /// The known dollars per (provider, model), unrounded.
    by_model: BTreeMap<String, f64>,
}

impl Priced {
    pub(super) fn total(&self) -> Option<f64> {
        (self.unpriced.is_empty() && (self.known || self.unpriced_empty.is_empty())).then_some(self.usd)
    }

    /// The pairs that make the cost unknown.
    pub(super) fn missing(&self) -> BTreeSet<String> {
        let mut out = self.unpriced.clone();
        if !self.known {
            out.extend(self.unpriced_empty.iter().cloned());
        }
        out
    }

    #[cfg(test)]
    pub(super) fn add_known(&mut self, usd: f64) {
        self.add(Some(TurnCost { usd, basis: None }), "p", "m", pricing::Tokens::default());
    }

    #[cfg(test)]
    pub(super) fn add_unknown(&mut self, pair: &str) {
        self.unpriced.insert(pair.into());
    }

    /// Another session's cost folded into this one: a parent and the
    /// subagents it spawned spend together.
    pub(super) fn merge(&mut self, o: Priced) {
        self.usd += o.usd;
        self.known |= o.known;
        self.unpriced.extend(o.unpriced);
        self.unpriced_empty.extend(o.unpriced_empty);
        self.bases.extend(o.bases);
        for (k, v) in o.by_model {
            *self.by_model.entry(k).or_default() += v;
        }
    }

    /// The dollars that could be priced, whatever could not be.
    pub(super) fn known_usd(&self) -> f64 {
        self.usd
    }

    fn add(&mut self, cost: Option<TurnCost>, provider: &str, model: &str, t: pricing::Tokens) {
        match cost {
            Some(TurnCost { usd, basis }) => {
                self.known = true;
                self.usd += usd;
                *self.by_model.entry(pair_name(provider, model)).or_default() += usd;
                if let Some(b) = basis {
                    self.bases.insert(b);
                }
            }
            None if t == pricing::Tokens::default() => {
                self.unpriced_empty.insert(pair_name(provider, model));
            }
            None => {
                self.unpriced.insert(pair_name(provider, model));
            }
        }
    }
}

/// One figure: dollars, and the price source when krowk priced it rather
/// than the source reporting it.
#[derive(Debug, Clone, Copy)]
pub(super) struct TurnCost {
    usd: f64,
    basis: Option<pricing::Basis>,
}

/// Claude writes `<synthetic>` where no model answered; that names no model.
fn real_model(model: &str) -> &str {
    if model.starts_with('<') && model.ends_with('>') { "" } else { model }
}

fn pair_name(provider: &str, model: &str) -> String {
    let model = real_model(model);
    match (provider.is_empty(), model.is_empty()) {
        (_, true) => format!("{} (no model recorded)", if provider.is_empty() { "unknown provider" } else { provider }),
        (true, false) => model.to_string(),
        _ => format!("{provider}/{model}"),
    }
}

/// Reported dollars win: the source knew what it was charged. A reported 0
/// is no report — opencode writes 0 for any model it cannot price — so it
/// falls through to the tokens, priced at current rates, or unknown.
fn cost_of(ctx: &Ctx, provider: &str, model: &str, reported_micros: Option<i64>, t: pricing::Tokens) -> Option<TurnCost> {
    if let Some(m) = reported_micros.filter(|m| *m > 0) {
        return Some(TurnCost { usd: m as f64 / 1e6, basis: None });
    }
    let model = real_model(model);
    if model.is_empty() {
        return None;
    }
    let (rates, basis) = pricing::price_with_basis(ctx.io.env, provider, model)?;
    Some(TurnCost { usd: rates.cost(t), basis: Some(basis) })
}

fn group_tokens(g: &CostGroup) -> pricing::Tokens {
    pricing::Tokens { input: g.input, output: g.output, cache_read: g.cache_read, cache_write: g.cache_write, reasoning: g.reasoning }
}

fn turn_tokens(t: &TurnDetail) -> pricing::Tokens {
    pricing::Tokens { input: t.input, output: t.output, cache_read: t.cache_read, cache_write: t.cache_write, reasoning: t.reasoning }
}

/// A session priced per (provider, model) its turns ran on. A session with
/// no turns costs what its own model says nothing costs: 0 when the model
/// has a price, unknown when it has none.
///
/// A session whose every turn is counted elsewhere (a ledger whose rows all
/// sit in transcripts) costs 0 here, and says where the cost went.
fn price_session(ctx: &Ctx, r: &SessionRow, groups: Option<&Vec<CostGroup>>) -> Priced {
    let groups: &[CostGroup] = groups.map_or(&[], Vec::as_slice);
    price_groups(ctx, &r.provider, &r.model, r.turn_count, groups)
}

/// The one roll-up both the listing and `show` use, so both sum the same
/// figures in the same order.
fn price_groups(ctx: &Ctx, provider: &str, model: &str, turn_count: i64, groups: &[CostGroup]) -> Priced {
    let mut p = Priced::default();
    if groups.is_empty() && turn_count > 0 {
        p.known = true;
        p.elsewhere = true;
        return p;
    }
    if groups.is_empty() {
        p.add(cost_of(ctx, provider, model, None, pricing::Tokens::default()), provider, model, pricing::Tokens::default());
    }
    for g in groups {
        p.add(cost_of(ctx, &g.provider, &g.model, g.reported.then_some(g.usd_micros), group_tokens(g)), &g.provider, &g.model, group_tokens(g));
    }
    p
}

fn turn_cost(ctx: &Ctx, t: &TurnDetail) -> Option<TurnCost> {
    cost_of(ctx, &t.provider, &t.model, t.usd_micros, turn_tokens(t))
}

/// A ledger turn counted elsewhere: in a transcript, or an earlier export.
pub(super) fn is_observed(t: &TurnDetail) -> bool {
    t.status == krowk_store::STATUS_OBSERVED || t.status == krowk_store::STATUS_DUPLICATE
}

/// The footnote under every priced listing: where the rates came from, and
/// what — means.
fn cost_footnote(bases: &BTreeSet<pricing::Basis>, dashes: bool) -> String {
    let note = pricing::basis_note(bases);
    let dash = "— has no price for its model";
    match (note.is_empty(), dashes) {
        (true, true) => dash.into(),
        (true, false) => String::new(),
        (false, true) => format!("costs {note}; {dash}"),
        (false, false) => format!("costs {note}"),
    }
}

fn session_row_json(r: &SessionRow, p: &Priced, now: i64) -> Value {
    let cost = p.total();
    let mut v = json!({
        "id": r.id,
        "title": r.title,
        "harness": r.harness,
        "model": r.model,
        "provider": r.provider,
        "turns": r.turn_count,
        "cost_usd": cost,
        "cost_display": cost_display(p),
        "time_updated_ms": r.time_updated,
        "time_updated_relative": relative_time(r.time_updated, now),
        "worktree": r.worktree_path,
    });
    if !r.foreign_session_id.is_empty() {
        v["foreign_session_id"] = json!(r.foreign_session_id);
    }
    if !r.directory.is_empty() {
        v["directory"] = json!(r.directory);
    }
    if p.total().is_none() {
        v["unpriced"] = json!(p.missing());
    }
    if p.elsewhere {
        v["cost_counted_elsewhere"] = json!(true);
    }
    v
}

/// A session's cost as text: the figure, —, or where it was counted.
fn cost_display(p: &Priced) -> String {
    if p.elsewhere {
        return "counted elsewhere".into();
    }
    p.total().map_or("—".to_string(), format_cost)
}

/// Cents from a cent up; below that, three significant digits, so $0.00007
/// and $0.00012 do not both read as $0.0001. Rounded here, once, at display.
fn format_cost(usd: f64) -> String {
    if usd >= 0.01 || usd <= 0.0 {
        return format!("${usd:.2}");
    }
    significant(usd)
}

/// `show`'s figures, where one session's split between models is the point:
/// three significant digits below a dollar, cents above.
pub(super) fn format_cost_precise(usd: f64) -> String {
    if usd >= 1.0 || usd <= 0.0 {
        return format!("${usd:.2}");
    }
    significant(usd)
}

fn significant(usd: f64) -> String {
    let places = (2 - usd.log10().floor() as i32).clamp(2, 12) as usize;
    format!("${usd:.places$}")
}

/// The human table's cost: exact 0 is free, dust collapses to <$0.01.
fn human_cost(usd: f64) -> String {
    if usd == 0.0 {
        "free".into()
    } else if usd > 0.0 && usd < 0.01 {
        "<$0.01".into()
    } else {
        format!("${usd:.2}")
    }
}

fn relative_time(ms: i64, now: i64) -> String {
    if ms > now {
        return "in the future".into();
    }
    let secs = (now - ms) / 1000;
    let (m, h, d) = (60, 3600, 86400);
    match secs {
        s if s < m => "just now".into(),
        s if s < h => format!("{}m ago", s / m),
        s if s < 24 * h => format!("{}h ago", s / h),
        s if s < 48 * h => "yesterday".into(),
        s if s < 30 * d => format!("{}d ago", s / d),
        s if s < 365 * d => format!("{}mo ago", s / (30 * d)),
        s => format!("{}y ago", s / (365 * d)),
    }
}

pub(super) fn cell(s: &str) -> String {
    termclean::cell(s)
}

/// Caps s at n chars, so a multi-byte character is never cut in half.
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n < 3 {
        return s.chars().take(n).collect();
    }
    s.chars().take(n - 3).collect::<String>() + "..."
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn paint(colour: bool, code: &str, s: &str) -> String {
    if !colour || s.is_empty() { s.to_string() } else { format!("\x1b[{code}m{s}\x1b[0m") }
}

fn pad_right(colour: bool, code: &str, s: &str, w: usize) -> String {
    paint(colour, code, s) + &" ".repeat(w.saturating_sub(width(s)))
}

fn pad_left(colour: bool, code: &str, s: &str, w: usize) -> String {
    " ".repeat(w.saturating_sub(width(s))) + &paint(colour, code, s)
}

pub(super) fn display_title(t: &str) -> String {
    if t.is_empty() { "(untitled)".into() } else { t.to_string() }
}

fn session_source(r: &SessionRow) -> String {
    let (harness, model) = (cell(&r.harness), cell(&r.model));
    match (harness.is_empty(), model.is_empty()) {
        (false, false) => format!("{harness} · {model}"),
        (false, true) => harness,
        _ => model,
    }
}

fn short_id(id: &str) -> String {
    cell(id).chars().take(8).collect()
}

fn human_sessions_list(rows: &[SessionRow], priced: &[Priced], colour: bool, now: i64) -> String {
    if rows.is_empty() {
        return "no sessions — run `krowk sessions import --from all`".into();
    }
    const MAX_TITLE: usize = 60;
    const MAX_SOURCE: usize = 40;
    let titles: Vec<String> = rows.iter().map(|r| display_title(&cell(&r.title))).collect();
    let sources: Vec<String> = rows.iter().map(|r| truncate_chars(&session_source(r), MAX_SOURCE)).collect();
    let tw = titles.iter().map(|t| width(t)).max().unwrap_or(0).max(width("Title")).min(MAX_TITLE);
    let sw = sources.iter().map(|s| width(s)).max().unwrap_or(0).max(width("Source")).min(MAX_SOURCE);
    let mut lines = vec![[
        pad_right(colour, "2", "Title", tw),
        pad_right(colour, "2", "Source", sw),
        pad_left(colour, "2", "Turns", 9),
        pad_left(colour, "2", "Cost", 10),
        paint(colour, "2", "Updated"),
    ]
    .join("  ")];
    for (i, r) in rows.iter().enumerate() {
        let title = truncate_chars(&titles[i], MAX_TITLE);
        let turns = format!("{:3} {}", r.turn_count, paint(colour, "2", if r.turn_count == 1 { "turn" } else { "turns" }));
        let (cost, code) = match priced[i].total() {
            Some(_) if priced[i].elsewhere => ("elsewhere".into(), "2"),
            Some(p) => (human_cost(p), if p == 0.0 { "2" } else { "32" }),
            None => ("—".into(), "2"),
        };
        let short = short_id(&r.id);
        let id = if short.is_empty() { String::new() } else { paint(colour, "2", &format!("#{short}")) };
        lines.push(
            [
                pad_right(colour, "1", &title, tw),
                pad_right(colour, "2", &sources[i], sw),
                turns,
                pad_left(colour, code, &cost, 10),
                paint(colour, "2", &relative_time(r.time_updated, now)),
                id,
            ]
            .join("  "),
        );
    }
    lines.join("\n")
}

/// The picker offers only rows already in the store, which is what makes it
/// safe to offer.
pub(super) fn pick_session(rows: &[SessionRow], now: i64) -> Result<String, Error> {
    let labels: Vec<String> = rows
        .iter()
        .map(|r| {
            let title = display_title(&cell(&r.title));
            let title = if width(&title) > 48 { title.chars().take(45).collect::<String>() + "..." } else { title };
            let hm = cell(format!("{} {}", r.harness.trim(), r.model.trim()).trim());
            let hm = if width(&hm) > 32 { hm.chars().take(29).collect::<String>() + "..." } else { hm };
            let mut label = title;
            if !hm.is_empty() {
                label += &format!("  —  {hm}");
            }
            label + &format!("  ·  {:<13}  ·  {}", relative_time(r.time_updated, now), short_id(&r.id))
        })
        .collect();
    let picked = inquire::Select::new("Pick a session", labels.clone())
        .raw_prompt()
        .map_err(|_| fail("selection_cancelled", "nothing was selected and nothing was changed"))?;
    let _ = labels;
    Ok(rows[picked.index].id.clone())
}

// ---- show -------------------------------------------------------------------

/// The one session `krowk sessions <verb> <id>` names, read in full.
pub(super) fn load_detail(ctx: &Ctx, args: &[String], verb: &str) -> Result<SessionDetail, Error> {
    let conn = open_store(ctx)?;
    let id = resolve_arg(ctx, &conn, args, verb)?;
    load_by_id(ctx, &conn, &id)
}

pub(super) fn load_by_id(ctx: &Ctx, conn: &Connection, id: &str) -> Result<SessionDetail, Error> {
    match krowk_store::load_session_detail(conn, id) {
        Ok(d) => Ok(d),
        Err(StoreError::NotFound(_)) => Err(fail("no_session", format!("no session {id:?}"))),
        Err(e) => Err(store_fail(&e, &db_path_string(ctx))),
    }
}

/// The session id `krowk sessions <verb> <id>` names.
pub(super) fn resolve_arg(ctx: &Ctx, conn: &Connection, args: &[String], verb: &str) -> Result<String, Error> {
    let Some(reference) = args.first().filter(|a| !a.trim().is_empty()) else {
        return Err(fail("no_session", format!("pass the session: `krowk sessions {verb} <id>`")));
    };
    if args.len() > 1 {
        return Err(fail("bad_flag", format!("`krowk sessions {verb}` takes one session id, got extra {}", args[1..].join(" "))));
    }
    match krowk_store::resolve_session_id(conn, reference) {
        Ok(id) => Ok(id),
        Err(StoreError::Ambiguous { message, .. }) => Err(fail("ambiguous_session", message)),
        Err(StoreError::NotFound(_)) => Err(fail(
            "no_session",
            format!("{:?} matches no session — pass a full id, an id prefix of at least 8 chars, or a foreign session id", reference.trim()),
        )),
        Err(e) => Err(store_fail(&e, &db_path_string(ctx))),
    }
}

/// What `refresh_session` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refresh {
    /// Every transcript behind the session was already in the store.
    Current,
    /// Something had moved, and was imported.
    Imported,
    /// The session's source cannot be re-read one session at a time (a
    /// Cursor or ledger session, or one with no foreign id).
    NotRefreshable,
}

/// How long a check waits for another import to finish before failing.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(15);

/// Re-reads the transcripts behind one or more sessions of one source — by
/// foreign id, and every subagent under them, however deep, found through
/// the source's own parent links (so one started since the last import is
/// included) — when they moved, so a check that must be current is. Nothing
/// else on the machine is read. What moved is found before the import lock
/// is taken, so a check with nothing to read never waits; one that must
/// write waits for another import to finish, and past that gives up with
/// `import_locked` rather than answer from a stale store.
pub(super) fn refresh_session(ctx: &Ctx, provider: &str, foreign_ids: &[String]) -> Result<Refresh, Error> {
    let foreign_ids: Vec<&String> = foreign_ids.iter().filter(|f| !f.is_empty()).collect();
    let Some(source) = all_sources().into_iter().find(|s| s.name() == provider) else { return Ok(Refresh::NotRefreshable) };
    if foreign_ids.is_empty() || matches!(provider, krowk_import::PROVIDER_LEDGER | krowk_import::PROVIDER_CURSOR) {
        return Ok(Refresh::NotRefreshable);
    }
    let store_path = resolve_store_path(ctx)?;
    let env = ctx.io.env;
    let refs = source.discover(env).map_err(|e| fail("import_failed", format!("{provider}: {e}")))?;
    // The sessions and every descendant, to a fixed point over the source's
    // parent links.
    let parents = source.parents(env, &refs);
    let mut ours: std::collections::HashSet<String> = foreign_ids.iter().map(|f| f.to_string()).collect();
    loop {
        let before = ours.len();
        for (child, parent) in &parents {
            if ours.contains(parent) {
                ours.insert(child.clone());
            }
        }
        if ours.len() == before {
            break;
        }
    }
    let moved = |conn: &Connection| -> Result<Vec<(Ref, String)>, Error> {
        let mut out = Vec::new();
        for r in refs.iter().filter(|r| ours.contains(&r.id)) {
            let stored = krowk_store::read_import_state(conn, &r.key()).map_err(|e| store_fail(&e, &store_path))?;
            if stored.is_empty() || !source.unchanged(env, r, &stored) {
                out.push((r.clone(), stored));
            }
        }
        Ok(out)
    };
    if moved(&open_store(ctx)?)?.is_empty() {
        return Ok(Refresh::Current);
    }
    let _lock = lock_store_waiting(&store_path, REFRESH_LOCK_WAIT)?;
    let conn = open_store(ctx)?;
    // Asked again under the lock: the import that held it may have done the work.
    let todo = moved(&conn)?;
    if todo.is_empty() {
        return Ok(Refresh::Current);
    }
    let writer = krowk_store::Writer::new(&conn);
    let mut links = Vec::new();
    for (r, stored) in &todo {
        let key = r.key();
        let (thread, next, _) = source.read(env, r, stored).map_err(|e| fail("import_failed", format!("{key}: {e}")))?;
        writer.ingest_with_cursor(&thread, &key, &next).map_err(|e| store_fail(&e, &store_path))?;
        if let Some(parent) = &thread.parent {
            links.push((thread.binding.clone(), parent.clone()));
        }
    }
    for (child, parent) in &links {
        writer.link_parent_later(child, parent).map_err(|e| store_fail(&e, &store_path))?;
    }
    krowk_store::reconcile_ledger(&conn).map_err(|e| store_fail(&e, &store_path))?;
    Ok(Refresh::Imported)
}

/// `lock_store`, waiting up to `wait` for an import that holds it.
fn lock_store_waiting(store_path: &str, wait: Duration) -> Result<std::fs::File, Error> {
    let started = Instant::now();
    loop {
        match try_lock_store(store_path)? {
            Some(f) => return Ok(f),
            None if started.elapsed() < wait => std::thread::sleep(Duration::from_millis(100)),
            None => {
                return Err(fail(
                    "import_locked",
                    format!("another import held the store for {}s, so the session could not be brought up to date — retry once it finishes", wait.as_secs()),
                ));
            }
        }
    }
}

pub fn show(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    check_os()?;
    #[allow(unused_mut)]
    let mut d = load_detail(ctx, args, "show")?;
    // Opening an archived session brings it back first (R-VINT-4).
    #[cfg(all(feature = "harness", unix))]
    if d.session.harness == krowk_harness::project::HARNESS && restore_if_archived(ctx, &d.session.foreign_session_id)? {
        let conn = open_store(ctx)?;
        d = load_by_id(ctx, &conn, &d.session.id)?;
    }
    if ctx.format != Format::Human {
        let (data, summary) = session_show_json(ctx, &d);
        return emit_data(ctx, data, summary);
    }
    let text = human_session_show(ctx, &d, ctx.f.thinking, now_ms());
    let _ = write!(ctx.io.stdout, "{text}");
    Ok(())
}

/// Each turn priced for display, and the session's total rolled up exactly
/// as the listing rolls it up: per (provider, model, reported), in that order.
pub(super) fn price_turns(ctx: &Ctx, d: &SessionDetail) -> (Vec<Option<TurnCost>>, Priced) {
    let costs: Vec<Option<TurnCost>> = d.turns.iter().map(|t| if is_observed(t) { None } else { turn_cost(ctx, t) }).collect();
    let mut groups: BTreeMap<(String, String, bool), CostGroup> = BTreeMap::new();
    for t in d.turns.iter().filter(|t| !is_observed(t)) {
        let reported = t.usd_micros.is_some_and(|m| m > 0);
        let g = groups.entry((t.provider.clone(), t.model.clone(), reported)).or_insert_with(|| CostGroup {
            provider: t.provider.clone(),
            model: t.model.clone(),
            reported,
            ..CostGroup::default()
        });
        g.turns += 1;
        g.input += t.input;
        g.output += t.output;
        g.cache_read += t.cache_read;
        g.cache_write += t.cache_write;
        g.reasoning += t.reasoning;
        if reported {
            g.usd_micros += t.usd_micros.unwrap_or(0);
        }
    }
    let groups: Vec<CostGroup> = groups.into_values().collect();
    let s = &d.session;
    (costs, price_groups(ctx, &s.provider, &s.model, d.turns.len() as i64, &groups))
}

fn session_show_json(ctx: &Ctx, d: &SessionDetail) -> (Value, String) {
    let (costs, total) = price_turns(ctx, d);
    let messages: Vec<Value> = d
        .messages
        .iter()
        .map(|m| {
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| {
                    let data = serde_json::from_str::<Value>(&p.data).unwrap_or_else(|_| Value::String(p.data.clone()));
                    let mut v = serde_json::Map::new();
                    v.insert("seq".into(), json!(p.seq));
                    v.insert("type".into(), json!(p.kind));
                    if !p.tool_call_id.is_empty() {
                        v.insert("tool_call_id".into(), json!(p.tool_call_id));
                    }
                    if p.kind == "tool_call" {
                        let name = krowk_store::tool_name_of(&p.data);
                        if !name.is_empty() {
                            v.insert("tool_name".into(), json!(name));
                        }
                    }
                    if p.kind == "tool_result" {
                        if !p.tool_name.is_empty() {
                            v.insert("tool_name".into(), json!(p.tool_name));
                        }
                        v.insert("linked".into(), json!(p.linked));
                    }
                    v.insert("data".into(), data);
                    Value::Object(v)
                })
                .collect();
            let mut v = serde_json::Map::new();
            v.insert("seq".into(), json!(m.seq));
            v.insert("role".into(), json!(m.role));
            if !m.provider.is_empty() {
                v.insert("provider".into(), json!(m.provider));
            }
            if !m.model.is_empty() {
                v.insert("model".into(), json!(m.model));
            }
            v.insert("parts".into(), Value::Array(parts));
            Value::Object(v)
        })
        .collect();
    let turns: Vec<Value> = d
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut v = serde_json::Map::new();
            v.insert("seq".into(), json!(t.seq));
            if !t.status.is_empty() {
                v.insert("status".into(), json!(t.status));
            }
            if !t.model.is_empty() {
                v.insert("provider".into(), json!(t.provider));
                v.insert("model".into(), json!(t.model));
            }
            v.insert("input_tokens".into(), json!(t.input));
            v.insert("output_tokens".into(), json!(t.output));
            v.insert("reasoning_tokens".into(), json!(t.reasoning));
            v.insert("cache_read_tokens".into(), json!(t.cache_read));
            v.insert("cache_write_tokens".into(), json!(t.cache_write));
            v.insert("total_tokens".into(), json!(t.total));
            if let Some(usd) = t.usd_micros {
                v.insert("cost_usd_micros".into(), json!(usd));
            }
            // Unrounded: the display rounds, once.
            match (is_observed(t), costs[i]) {
                (true, _) => v.insert("cost_counted_elsewhere".into(), json!(true)),
                (false, Some(c)) => {
                    v.insert("cost_usd".into(), json!(c.usd));
                    v.insert("cost_source".into(), json!(if c.basis.is_some() { "priced" } else { "reported" }))
                }
                (false, None) => v.insert("cost_usd".into(), Value::Null),
            };
            Value::Object(v)
        })
        .collect();
    let s = &d.session;
    let mut out = serde_json::Map::new();
    for (k, v) in [("id", &s.id), ("title", &s.title), ("harness", &s.harness), ("model", &s.model), ("provider", &s.provider), ("worktree", &s.worktree_path)] {
        out.insert(k.into(), json!(v));
    }
    if !s.directory.is_empty() {
        out.insert("directory".into(), json!(s.directory));
    }
    if !s.foreign_session_id.is_empty() {
        out.insert("foreign_session_id".into(), json!(s.foreign_session_id));
    }
    let summary = format!("{} — {} turns, {} messages", display_title(&cell(&s.title)), turns.len(), messages.len());
    out.insert("turns".into(), Value::Array(turns));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("cost_usd".into(), json!(total.total()));
    out.insert("cost_display".into(), json!(cost_display(&total)));
    if total.elsewhere {
        out.insert("cost_counted_elsewhere".into(), json!(true));
    }
    if total.total().is_none() {
        out.insert("unpriced".into(), json!(total.missing()));
    }
    if total.by_model.len() > 1 {
        out.insert("cost_by_model".into(), json!(total.by_model));
    }
    if !total.bases.is_empty() {
        out.insert("priced_with".into(), json!(pricing::basis_note(&total.bases)));
    }
    (Value::Object(out), summary)
}

/// Every stored string printed here is transcript text, so each goes
/// through the terminal scrubber before it reaches the screen.
fn human_session_show(ctx: &Ctx, d: &SessionDetail, show_thinking: bool, now: i64) -> String {
    let s = &d.session;
    let mut b = format!("{}\n", display_title(&cell(&s.title)));
    let mut meta = cell(&s.harness);
    let model = cell(&s.model);
    if !model.is_empty() {
        meta += &format!("  {model}");
    }
    let (costs, total) = price_turns(ctx, d);
    meta += &format!("  {}", if total.elsewhere { "counted elsewhere".to_string() } else { total.total().map_or("—".to_string(), format_cost_precise) });
    meta += &format!("  {}", relative_time(s.time_updated, now));
    for extra in [cell(&s.worktree_path), cell(&s.directory)] {
        if !extra.is_empty() {
            meta += &format!("\n{extra}");
        }
    }
    b += &meta;
    b += "\n";
    for (i, t) in d.turns.iter().enumerate() {
        b += &format!("\nturn {}", t.seq);
        let st = cell(&t.status);
        if !st.is_empty() {
            b += &format!("  {st}");
        }
        let model = cell(&t.model);
        if !model.is_empty() && model != cell(&s.model) {
            b += &format!("  {model}");
        }
        b += &format!("  {} tokens", t.total);
        b += &match (is_observed(t), costs[i]) {
            (true, _) => "  counted elsewhere".to_string(),
            (false, Some(c)) => format!("  {}{}", format_cost_precise(c.usd), if c.basis.is_none() { " reported" } else { "" }),
            (false, None) => "  —".to_string(),
        };
        b += "\n";
    }
    if total.by_model.len() > 1 {
        let parts: Vec<String> = total.by_model.iter().map(|(m, usd)| format!("{} {}", cell(m), format_cost_precise(*usd))).collect();
        b += &format!("\nby model  {}\n", parts.join("  ·  "));
    }
    if !d.turns.is_empty() {
        let dashes = total.total().is_none() || costs.iter().zip(&d.turns).any(|(c, t)| c.is_none() && !is_observed(t));
        let note = cost_footnote(&total.bases, dashes);
        if !note.is_empty() {
            b += &format!("\n{}\n", paint(ctx.colour, "2", &note));
        }
    }
    for m in &d.messages {
        b += &format!("\n[{}]\n", cell(&m.role));
        for p in &m.parts {
            b += &human_part(p, show_thinking);
            b += "\n";
        }
    }
    b.trim_end_matches('\n').to_string() + "\n"
}

fn one_line(s: &str, n: usize) -> String {
    truncate_chars(&cell(s), n)
}

fn fields(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn human_part(p: &krowk_store::PartDetail, show_thinking: bool) -> String {
    let parsed = serde_json::from_str::<Value>(&p.data).ok();
    let str_field = |k: &str| parsed.as_ref().and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or_default().to_string();
    // A field as text: a JSON string unquoted, anything else as its JSON.
    let raw_field = |k: &str| match parsed.as_ref().and_then(|v| v.get(k)) {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    };
    match p.kind.as_str() {
        "text" => cell(&if parsed.is_some() { str_field("text") } else { fields(&p.data) }),
        "thinking" => {
            let text = if parsed.is_none() {
                fields(&p.data)
            } else {
                let t = str_field("thinking");
                if t.is_empty() { str_field("text") } else { t }
            };
            if show_thinking {
                format!("thinking: {}", text.split('\n').map(termclean::cell).collect::<Vec<_>>().join("\n"))
            } else {
                format!("thinking: {} (use --thinking for all)", one_line(&text, 100))
            }
        }
        "tool_call" => {
            let name = cell(&krowk_store::tool_name_of(&p.data));
            let name = if name.is_empty() { "unknown tool".to_string() } else { name };
            let input = raw_field("input");
            if input.is_empty() { format!("tool {name}") } else { format!("tool {name}: {}", one_line(&input, 200)) }
        }
        "tool_result" => {
            let name = cell(&p.tool_name);
            let name = if name.is_empty() { "unknown tool".to_string() } else { name };
            let out = raw_field("output");
            if out.is_empty() { format!("result ({name})") } else { format!("result ({name}): {}", one_line(&out, 300)) }
        }
        _ => format!("{}: {}", cell(&p.kind), one_line(&fields(&p.data), 200)),
    }
}

// ---- import / rebuild / sync ------------------------------------------------

#[derive(Debug, Default, Serialize)]
struct ProviderReport {
    provider: String,
    files: usize,
    sessions_seen: usize,
    messages_seen: usize,
    parts_seen: usize,
    sessions_inserted: usize,
    messages_inserted: usize,
    parts_inserted: usize,
    skipped_by_type: BTreeMap<String, usize>,
    skipped_lines: usize,
    files_failed: usize,
    files_unchanged: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
    errors_truncated: usize,
    duration_ms: u128,
    #[serde(skip)]
    discover_failed: bool,
}

#[derive(Debug, Serialize)]
struct SyncPricing {
    status: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    warning: String,
}

/// What reconciling the provider ledgers found: rows a transcript accounts
/// for, rows only the provider saw — billed executions the client never
/// received an answer for — and rows another export already holds.
#[derive(Debug, Serialize)]
struct LedgerReport {
    observed: usize,
    unobserved: usize,
    duplicate: usize,
}

#[derive(Debug, Default, Serialize)]
struct ImportReport {
    dry_run: bool,
    store: String,
    providers: Vec<ProviderReport>,
    duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    removed: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing: Option<SyncPricing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger: Option<LedgerReport>,
    /// Every (provider, model) in the store with tokens krowk cannot price
    /// and no cost the source reported: their sessions' costs read —.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unpriced_models: Vec<String>,
}

impl ProviderReport {
    fn record(&mut self, reason: String) {
        self.files_failed += 1;
        if self.errors.len() >= MAX_REPORTED_ERRORS {
            self.errors_truncated += 1;
            return;
        }
        self.errors.push(truncate_for_report(&reason, MAX_ERROR_REASON_LEN));
    }

    fn count(&mut self, r: &krowk_store::IngestResult) {
        self.sessions_seen += r.sessions.inserted + r.sessions.skipped;
        self.messages_seen += r.messages.inserted + r.messages.skipped;
        self.parts_seen += r.parts.inserted + r.parts.skipped;
        self.sessions_inserted += r.sessions.inserted;
        self.messages_inserted += r.messages.inserted;
        self.parts_inserted += r.parts.inserted;
    }

    /// Folds one read's lossiness in, bounded: at most 32 named types (in
    /// sorted order, so the same 32 every run), each name at most 64 bytes;
    /// the rest summed under `krowk:other`, which sits beside them.
    fn absorb(&mut self, r: &krowk_import::ReadResult) {
        for (k, v) in &r.unknown_types {
            let known = self.skipped_by_type.contains_key(k);
            let named = self.skipped_by_type.len() - usize::from(self.skipped_by_type.contains_key(SKIPPED_TYPE_OTHER));
            let key = if k.len() > MAX_SKIPPED_TYPE_LEN || k == SKIPPED_TYPE_OTHER || (!known && named >= MAX_SKIPPED_TYPES) {
                SKIPPED_TYPE_OTHER
            } else {
                k.as_str()
            };
            *self.skipped_by_type.entry(key.to_string()).or_default() += v;
        }
        self.skipped_lines += r.skipped_count;
    }
}

fn truncate_for_report(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max - '…'.len_utf8();
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

fn selected_sources(from: &str) -> Result<Vec<Box<dyn Source>>, Error> {
    let all = all_sources();
    let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
    let choices = format!("{}|all", names.join("|"));
    match from {
        "" => Err(fail("bad_flag", format!("`krowk sessions import` needs --from <{choices}> — it says which agent's transcripts to read"))),
        "all" => Ok(all),
        _ => all
            .into_iter()
            .find(|s| s.name() == from)
            .map(|s| vec![s])
            .ok_or_else(|| fail("bad_flag", format!("--from {from} is not a source krowk can read — one of {choices}"))),
    }
}

pub fn import(ctx: &mut Ctx) -> Result<(), Error> {
    // Before anything touches a path: the store would otherwise be created
    // on a machine about to be told the command does not run there.
    check_os()?;
    let sources = selected_sources(&ctx.f.from)?;
    if ctx.f.limit < 0 {
        return Err(fail("bad_flag", "--limit is a maximum, so it cannot be negative — use 0 for no limit"));
    }
    let store_path = resolve_store_path(ctx)?;
    if ctx.f.dry_run {
        return import_into(ctx, None, &store_path, &sources, false, ImportReport::default());
    }
    // Taken before open: opening is itself a write.
    let _lock = lock_store(&store_path)?;
    let conn = open_store(ctx)?;
    import_into(ctx, Some(&conn), &store_path, &sources, false, ImportReport::default())
}

pub fn rebuild(ctx: &mut Ctx) -> Result<(), Error> {
    check_os()?;
    let store_path = resolve_store_path(ctx)?;
    let prompt = !ctx.f.yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) && interactive(ctx);
    if !ctx.f.yes && !prompt {
        return Err(fail(
            "confirmation_required",
            format!(
                "rebuilding deletes {store_path} and re-imports every transcript — run `krowk sessions rebuild --yes` to confirm when nobody is at a terminal to ask"
            ),
        ));
    }
    // Asked before the lock, so a question left on screen holds off no import.
    if prompt {
        let ok = inquire::Confirm::new(&format!("Delete {store_path} and re-import every transcript?")).with_default(false).prompt();
        if !matches!(ok, Ok(true)) {
            return Err(fail("selection_cancelled", "nothing was deleted"));
        }
    }
    let _lock = lock_store(&store_path)?;
    // Exactly the database and its WAL sidecars: import.lock is held right
    // now, and anything else in the directory is not krowk's to remove.
    let mut removed = Vec::new();
    for path in [store_path.clone(), format!("{store_path}-wal"), format!("{store_path}-shm")] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail("store_unavailable", format!("remove {path}: {e}"))),
        }
    }
    let conn = open_store(ctx)?;
    let sources = all_sources();
    import_into(ctx, Some(&conn), &store_path, &sources, false, ImportReport { removed: Some(removed), ..ImportReport::default() })
}

pub fn sync(ctx: &mut Ctx) -> Result<(), Error> {
    check_os()?;
    let store_path = resolve_store_path(ctx)?;
    let _lock = lock_store(&store_path)?;
    let conn = open_store(ctx)?;
    // Inside the lock, so "is a sync running" has one answer; bounded by the
    // refresh's own timeout.
    let p = sync_prices(ctx.io.env, ctx.f.no_network);
    if !p.warning.is_empty() && ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stderr, "! {}", p.warning);
    }
    let sources = all_sources();
    import_into(ctx, Some(&conn), &store_path, &sources, true, ImportReport { pricing: Some(p), ..ImportReport::default() })
}

/// Refreshes the models.dev cache when its last fetch is a day old, or
/// unknown, or in the future, or there is no readable cache. The sidecar's
/// `fetched_at_ms` is the clock: a 304 stamps it too, since "when did krowk
/// last ask" is the question.
fn sync_prices(env: &dyn Fn(&str) -> String, no_network: bool) -> SyncPricing {
    sync_prices_from(env, no_network, pricing::MODELS_URL, now_ms())
}

fn sync_prices_from(env: &dyn Fn(&str) -> String, no_network: bool, url: &str, now: i64) -> SyncPricing {
    let failed = |w: String| SyncPricing { status: "failed", warning: w };
    if no_network {
        return SyncPricing { status: "no_network", warning: String::new() };
    }
    if pricing::cache_path(env).is_none() {
        return failed("prices were not refreshed: no cache directory in the environment".into());
    }
    let fetched = pricing::Freshness::of(env).fetched_at_ms().filter(|f| *f <= now);
    if fetched.is_some_and(|f| now - f < PRICING_MAX_AGE.as_millis() as i64) {
        return SyncPricing { status: "fresh", warning: String::new() };
    }
    match pricing::refresh_within(env, url, Duration::from_secs(3)) {
        Err(e) => failed(format!("prices were not refreshed: {e}")),
        Ok(pricing::Outcome::Refreshed) => SyncPricing { status: "refreshed", warning: String::new() },
        Ok(pricing::Outcome::Unchanged) => SyncPricing { status: "unchanged", warning: String::new() },
        Ok(pricing::Outcome::Unreachable(why)) => {
            failed(format!("prices were not refreshed: {why} — the cache or the snapshot still prices everything"))
        }
    }
}

/// The import itself, shared by import, rebuild and sync; `report` carries
/// what they did first. One unreadable file is counted and the run goes on;
/// a source that could not be listed, or lost every file, fails the run
/// after every source has had its turn.
fn import_into(ctx: &mut Ctx, conn: Option<&Connection>, store_path: &str, sources: &[Box<dyn Source>], skip_unchanged: bool, mut report: ImportReport) -> Result<(), Error> {
    let started = Instant::now();
    report.dry_run = conn.is_none();
    report.store = store_path.to_string();
    let mut broken = Vec::new();
    for s in sources {
        let row = run_source(ctx, conn, store_path, s.as_ref(), skip_unchanged);
        if row.discover_failed {
            broken.push(format!("{} could not be listed", row.provider));
        } else if row.files > 0 && row.files_failed == row.files {
            broken.push(format!("{} lost every one of its {} transcripts", row.provider, row.files));
        }
        report.providers.push(row);
    }
    // After every source, whichever were read: a transcript imported after
    // its ledger row is what turns that row observed.
    if let Some(conn) = conn {
        match krowk_store::reconcile_ledger(conn) {
            Ok(r) if r.observed + r.unobserved + r.duplicate > 0 => {
                report.ledger = Some(LedgerReport { observed: r.observed, unobserved: r.unobserved, duplicate: r.duplicate })
            }
            Ok(_) => {}
            Err(e) => broken.push(format!("the provider ledgers could not be reconciled: {}", sanitize_store_err(e.message(), store_path))),
        }
        match unpriced_models(ctx, conn) {
            Ok(u) => report.unpriced_models = u,
            Err(e) => broken.push(format!("prices could not be checked: {}", sanitize_store_err(e.message(), store_path))),
        }
    }
    report.duration_ms = started.elapsed().as_millis();
    emit_import_report(ctx, &report)?;
    if !broken.is_empty() {
        return Err(fail("import_failed", format!("{} — the reasons are in `errors` in the report above", broken.join("; "))));
    }
    Ok(())
}

/// The pairs with tokens no price covers, across the whole store — so a model the
/// cache does not know yet is named at import instead of costing a silent 0.
fn unpriced_models(ctx: &Ctx, conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut out = BTreeSet::new();
    for g in krowk_store::cost_groups(conn, &[])?.values().flatten() {
        if group_tokens(g) != pricing::Tokens::default() && cost_of(ctx, &g.provider, &g.model, g.reported.then_some(g.usd_micros), group_tokens(g)).is_none() {
            out.insert(pair_name(&g.provider, &g.model));
        }
    }
    Ok(out.into_iter().collect())
}

fn run_source(ctx: &Ctx, conn: Option<&Connection>, store_path: &str, s: &dyn Source, skip_unchanged: bool) -> ProviderReport {
    let started = Instant::now();
    let env = ctx.io.env;
    let mut out = ProviderReport { provider: s.name().to_string(), ..ProviderReport::default() };
    let finish = |mut out: ProviderReport| {
        out.duration_ms = started.elapsed().as_millis();
        out
    };
    let mut refs: Vec<Ref> = match s.discover(env) {
        Ok(refs) => refs,
        Err(e) => {
            out.discover_failed = true;
            out.errors.push(truncate_for_report(&format!("discover: {e}"), MAX_ERROR_REASON_LEN));
            return finish(out);
        }
    };
    // Per source, and on refs: one session file or one session row.
    if ctx.f.limit > 0 {
        refs.truncate(ctx.f.limit as usize);
    }
    out.files = refs.len();
    let Some(conn) = conn else {
        return finish(out);
    };
    let writer = krowk_store::Writer::new(conn);
    let mut links = Vec::new();
    for r in &refs {
        let key = r.key();
        let stored = match krowk_store::read_import_state(conn, &key) {
            Ok(c) => c,
            Err(e) => {
                out.record(format!("{key}: {}", sanitize_store_err(e.message(), store_path)));
                continue;
            }
        };
        // A skipped ref is never re-read, so sync does not backfill what a
        // reader upgrade would now extract; import and rebuild do.
        if skip_unchanged && !stored.is_empty() && s.unchanged(env, r, &stored) {
            out.files_unchanged += 1;
            continue;
        }
        // A failed read is not ingested and writes no cursor: its turn list
        // may be truncated, and the next run retries from the same watermark.
        let (thread, next, res) = match s.read(env, r, &stored) {
            Ok(read) => read,
            Err(e) => {
                out.record(format!("{key}: {e}"));
                continue;
            }
        };
        out.absorb(&res);
        match writer.ingest_with_cursor(&thread, &key, &next) {
            Ok(ing) => {
                out.count(&ing);
                if let Some(parent) = &thread.parent {
                    links.push((thread.binding.clone(), parent.clone()));
                }
            }
            Err(e) => out.record(format!("{key}: {}", sanitize_store_err(e.message(), store_path))),
        }
    }
    // A child read before its parent (opencode lists newest first) is
    // linked now that the parent is in.
    for (child, parent) in &links {
        if let Err(e) = writer.link_parent_later(child, parent) {
            out.record(format!("{}: {}", child.foreign_session_id, sanitize_store_err(e.message(), store_path)));
        }
    }
    finish(out)
}

fn emit_import_report(ctx: &mut Ctx, report: &ImportReport) -> Result<(), Error> {
    if ctx.format != Format::Human {
        let data = serde_json::to_value(report).expect("report serializes");
        return emit_data(ctx, data, import_summary(report));
    }
    let mut out = String::new();
    for path in report.removed.iter().flatten() {
        out += &format!("removed   {path}\n");
    }
    if let Some(p) = &report.pricing {
        out += &format!("pricing   {}\n", p.status);
    }
    for p in &report.providers {
        out += &human_provider_line(p, report.dry_run);
        out += "\n";
        for e in &p.errors {
            out += &format!("  ! {e}\n");
        }
        if p.errors_truncated > 0 {
            out += &format!("  ! ... and {} more not shown\n", p.errors_truncated);
        }
    }
    if !report.unpriced_models.is_empty() {
        out += &format!("! no price for {} — their costs show as —; `krowk pricing refresh` may know them\n", report.unpriced_models.join(", "));
    }
    if let Some(l) = &report.ledger {
        out += &format!("reconciled {} ledger rows a transcript saw, {} only the provider saw", l.observed, l.unobserved);
        if l.duplicate > 0 {
            out += &format!(", {} already in another export", l.duplicate);
        }
        out += "\n";
    }
    let _ = write!(ctx.io.stdout, "{out}");
    Ok(())
}

fn human_provider_line(p: &ProviderReport, dry_run: bool) -> String {
    if dry_run {
        return format!("{:<9} {} files found (dry run, nothing written)  {}ms", p.provider, p.files, p.duration_ms);
    }
    let mut line = format!(
        "{:<9} {} files  {} sessions read  {} messages read  {} parts read  {} messages new  {}ms",
        p.provider, p.files, p.sessions_seen, p.messages_seen, p.parts_seen, p.messages_inserted, p.duration_ms
    );
    if p.files_unchanged > 0 {
        line += &format!("  ({} unchanged)", p.files_unchanged);
    }
    if p.files_failed > 0 {
        line += &format!("  ({} failed)", p.files_failed);
    }
    line
}

fn import_summary(r: &ImportReport) -> String {
    let files: usize = r.providers.iter().map(|p| p.files).sum();
    if r.dry_run {
        return format!("{files} files found, nothing written");
    }
    let sessions: usize = r.providers.iter().map(|p| p.sessions_seen).sum();
    let messages: usize = r.providers.iter().map(|p| p.messages_seen).sum();
    let inserted: usize = r.providers.iter().map(|p| p.messages_inserted).sum();
    let summary = format!("{files} files, {sessions} sessions read, {messages} messages read, {inserted} messages new");
    match &r.removed {
        Some(removed) => format!("removed {} files, then {summary}", removed.len()),
        None => summary,
    }
}

// ---- the import lock --------------------------------------------------------

/// import.lock beside krowk.db, flock'd exclusively and never waited on: a
/// second import is a mistake to report, not a queue to join. The kernel
/// drops the lock however the process dies, so a killed import leaves a
/// stale file and no stale lock. Dropping the returned file releases it.
fn lock_store(store_path: &str) -> Result<std::fs::File, Error> {
    try_lock_store(store_path)?.ok_or_else(|| {
        let path = Path::new(store_path).parent().unwrap_or(Path::new(".")).join("import.lock");
        fail(
            "import_locked",
            format!(
                "another `krowk sessions import` or `rebuild` is running on this store — wait for it to finish, or check {} if you think it is not",
                path.display()
            ),
        )
    })
}

/// The lock, or None when somebody else holds it; an error only for a lock
/// that could not be taken at all.
fn try_lock_store(store_path: &str) -> Result<Option<std::fs::File>, Error> {
    let dir = Path::new(store_path).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode_private()
        .create(&dir)
        .map_err(|e| fail("store_unavailable", e.to_string()))?;
    let path = dir.join("import.lock");
    match lock_file(&path) {
        Ok(f) => Ok(Some(f)),
        Err(None) => Ok(None),
        Err(Some(why)) => Err(fail("import_locked", format!("the import lock at {} could not be taken: {why}", path.display()))),
    }
}

trait PrivateDir {
    fn mode_private(&mut self) -> &mut Self;
}

impl PrivateDir for std::fs::DirBuilder {
    fn mode_private(&mut self) -> &mut Self {
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(self, 0o700);
        self
    }
}

/// Err(None) is somebody else holding it; Err(Some(why)) is a file that
/// could not be opened, or is not a lock krowk will take.
#[cfg(unix)]
fn lock_file(path: &Path) -> Result<std::fs::File, Option<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW and the regular-file check: a symlink or a fifo standing at
    // the path is not the thing being serialised.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| Some(format!("open {}: {e}", path.display())))?;
    let meta = f.metadata().map_err(|e| Some(format!("stat {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Some(format!("{} is not a regular file, so it is not a lock krowk will take", path.display())));
    }
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(None),
        Err(std::fs::TryLockError::Error(e)) => Err(Some(format!("lock {}: {e}", path.display()))),
    }
}

#[cfg(not(unix))]
fn lock_file(path: &Path) -> Result<std::fs::File, Option<String>> {
    Err(Some(format!("{}: locking is not supported here", path.display())))
}

// ---- pricing refresh --------------------------------------------------------

pub fn pricing_refresh(ctx: &mut Ctx) -> Result<(), Error> {
    let outcome = pricing::refresh(ctx.io.env, "").map_err(|e| fail("pricing_failed", e))?;
    let refreshed = outcome == pricing::Outcome::Refreshed;
    let path = pricing::cache_path(ctx.io.env).unwrap_or_default();
    if ctx.format != Format::Human {
        let fresh = pricing::Freshness::of(ctx.io.env);
        let mut report = json!({
            "meta_path": pricing::meta_path(&path).display().to_string(),
            "path": path.display().to_string(),
            "refreshed": refreshed,
            "snapshot_date": pricing::SNAPSHOT_DATE,
            "source": pricing::MODELS_URL,
            "fetched_at_ms": fresh.fetched_at_ms(),
            "age_days": fresh.age_days(now_ms()),
            "outcome": match &outcome {
                pricing::Outcome::Refreshed => "refreshed",
                pricing::Outcome::Unchanged => "unchanged",
                pricing::Outcome::Unreachable(_) => "unreachable",
            },
        });
        if let pricing::Outcome::Unreachable(why) = &outcome {
            report["warning"] = json!(why);
        }
        return ctx.emit(&output::encode(&report));
    }
    let _ = match &outcome {
        pricing::Outcome::Refreshed => writeln!(ctx.io.stdout, "prices refreshed from {}", pricing::MODELS_URL),
        pricing::Outcome::Unchanged => writeln!(ctx.io.stdout, "prices unchanged — models.dev confirmed the cache"),
        pricing::Outcome::Unreachable(why) => writeln!(ctx.io.stdout, "prices not refreshed: {why}"),
    };
    let _ = writeln!(ctx.io.stdout, "cache: {}", path.display());
    let _ = writeln!(ctx.io.stdout, "{}", pricing::Freshness::of(ctx.io.env).describe(now_ms()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_types_are_capped_and_the_overflow_is_summed() {
        let mut row = ProviderReport::default();
        let mut res = krowk_import::ReadResult::default();
        for i in 0..40 {
            res.unknown_types.insert(format!("t{i:02}"), 1);
        }
        res.unknown_types.insert("x".repeat(65), 2);
        row.absorb(&res);
        assert_eq!(row.skipped_by_type.len(), 33);
        assert_eq!(row.skipped_by_type[SKIPPED_TYPE_OTHER], 8 + 2);
    }

    #[test]
    fn a_second_lock_on_the_same_store_is_refused_as_held() {
        let dir = std::env::temp_dir().join(format!("krowk-lock-{}", std::process::id()));
        let store = dir.join("krowk.db").display().to_string();
        let first = lock_store(&store).unwrap();
        let second = lock_store(&store).unwrap_err();
        assert_eq!(second.code(), "import_locked");
        assert!(second.fix().starts_with("another"));
        drop(first);
        assert!(lock_store(&store).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_time_and_costs_read_as_go_wrote_them() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time(now - 90_000, now), "1m ago");
        assert_eq!(relative_time(now - 36 * 3_600_000, now), "yesterday");
        assert_eq!(relative_time(now + 1, now), "in the future");
        assert_eq!((format_cost(0.005), human_cost(0.005), human_cost(0.0)), ("$0.00500".into(), "<$0.01".into(), "free".into()));
        assert_eq!((format_cost(0.000_07), format_cost(0.000_12), format_cost(0.012_3)), ("$0.0000700".into(), "$0.000120".into(), "$0.01".into()));
        assert_eq!(truncate_chars("ąčęėįšųū", 5), "ąč...");
    }

    /// A models.dev stand-in answering each connection in turn: 200 with the
    /// body and an ETag, or 304 when the request carries that ETag.
    fn models_dev(connections: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api.json", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..connections {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = conn.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let body = r#"{"p":{"models":{"m":{"cost":{"input":1,"output":2}}}}}"#;
                let answer = if req.contains("if-none-match: \"v1\"") {
                    "HTTP/1.1 304 Not Modified\r\netag: \"v1\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                } else {
                    format!("HTTP/1.1 200 OK\r\netag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                };
                conn.write_all(answer.as_bytes()).unwrap();
                seen.push(req);
            }
            seen
        });
        (url, handle)
    }

    #[test]
    fn a_lost_or_corrupt_cache_is_fetched_whole_whatever_its_sidecar_says() {
        let dir = std::env::temp_dir().join(format!("krowk-sync-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache_home = dir.join("home").display().to_string();
        let env = move |k: &str| if k == "KROWK_HOME" { cache_home.clone() } else { String::new() };
        let cache = pricing::cache_path(&env).unwrap();
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        // A sidecar stamped just now, with the ETag, beside a file that holds nothing.
        std::fs::write(&cache, "garbage").unwrap();
        std::fs::write(pricing::meta_path(&cache), format!(r#"{{"etag":"\"v1\"","fetched_at_ms":{}}}"#, now_ms())).unwrap();
        assert_eq!(pricing::Freshness::of(&env), pricing::Freshness::Unreadable);
        let (url, server) = models_dev(1);
        assert_eq!(sync_prices_from(&env, false, &url, now_ms()).status, "refreshed", "no 304 for a file with no prices");
        assert!(!server.join().unwrap()[0].contains("if-none-match"), "the ETag was not sent");
        // A sidecar from the future is a clock that is off: no age, so due.
        std::fs::write(pricing::meta_path(&cache), format!(r#"{{"etag":"","fetched_at_ms":{}}}"#, now_ms() + 86_400_000 * 400)).unwrap();
        assert_eq!(pricing::Freshness::of(&env).age_days(now_ms()), None);
        assert!(pricing::Freshness::of(&env).describe(now_ms()).contains("clock is off"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_refreshes_a_stale_price_cache_and_leaves_a_fresh_one_alone() {
        let dir = std::env::temp_dir().join(format!("krowk-sync-prices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache_home = dir.join("home").display().to_string();
        let env = move |k: &str| if k == "KROWK_HOME" { cache_home.clone() } else { String::new() };
        let cache = pricing::cache_path(&env).unwrap();
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, r#"{"p":{"m":{"input":9}}}"#).unwrap();
        let day = 86_400_000;
        let now = now_ms();
        std::fs::write(pricing::meta_path(&cache), format!(r#"{{"etag":"","fetched_at_ms":{}}}"#, now - 2 * day)).unwrap();
        assert_eq!(pricing::Freshness::of(&env).age_days(now), Some(2));

        let (url, server) = models_dev(2);
        assert_eq!(sync_prices_from(&env, false, &url, now).status, "refreshed", "two days old is due");
        assert_eq!(pricing::Freshness::of(&env).age_days(now_ms()), Some(0));
        assert_eq!(sync_prices_from(&env, false, &url, now_ms()).status, "fresh", "just fetched is left alone");
        // A day on, the same prices: a 304 still counts as having asked.
        assert_eq!(sync_prices_from(&env, false, &url, now_ms() + day + 1).status, "unchanged");
        let seen = server.join().unwrap();
        assert!(seen[1].contains("if-none-match: \"v1\""), "the refresh is conditional: {seen:?}");
        assert!(pricing::Freshness::of(&env).describe(now_ms()).ends_with(", today"));
        assert_eq!(sync_prices_from(&env, true, &url, now).status, "no_network");
        let got: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(got["p"]["models"]["m"]["cost"], serde_json::json!({ "input": 1, "output": 2 }), "the stale file was replaced");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
