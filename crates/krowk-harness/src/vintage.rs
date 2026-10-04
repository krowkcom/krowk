//! Vintages: native sessions that have gone quiet leave the machine as one
//! sealed bundle per ISO week, and come back when opened (canon,
//! engineering/harness.md → Vintages).
//!
//! **Archiving** (`archive`) takes every native session idle past the
//! cut-off (14 days by default), groups them by the ISO week of their last
//! event, and for each week writes the week's vintage: JSONL, one line per
//! session holding its `events.jsonl` and `context.jsonl` verbatim,
//! zstd-compressed and sealed under the account key (`e2e::seal_vintage`)
//! (R-VINT-1). A week that already has a vintage is read, opened and merged
//! first, and the new one names the one it replaces, so two machines
//! archiving the same week never drop each other's sessions: the registry
//! refuses the second as `vintage_conflict`, and it reads again. A pinned
//! session is never taken, and neither is a subagent of one, or a session
//! another krowk holds open (R-VINT-2).
//!
//! **Only once the registry has finalized the vintage** does the session's
//! directory change: an `archived.json` stub (`Stub`) goes in — title,
//! summary, directory, dates, models and every turn's tokens, which is what
//! the listing, search and cost read (R-VINT-3) — and the two log files go.
//!
//! **Restoring** (`restore`) is the reverse: the stub's week is fetched,
//! checked against its digest, opened and unpacked, the session's two files
//! are written back, and the stub goes (R-VINT-4).

use crate::log::{self, CONTEXT_FILE, EVENTS_FILE};
use crate::protocol::LogEvent;
use krowk_api::Client;
use krowk_client::e2e::{self, AccountKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The stub an archived session leaves in its directory.
pub const STUB_FILE: &str = "archived.json";
/// A marker file: the session is pinned and never archived.
pub const PIN_FILE: &str = "pinned";
/// When the weekly job last ran, in ms since the epoch, in `sessions/`.
const STAMP_FILE: &str = ".archived-at";

/// Sessions idle past this long are archived, unless told otherwise.
pub const DEFAULT_IDLE_DAYS: u64 = 14;
/// How often the weekly job archives.
pub const WEEK_MS: i64 = 7 * DAY_MS;
const DAY_MS: i64 = 86_400_000;

/// The most a vintage may decompress to. A week of one person's sessions is
/// far below this; a frame claiming more is refused, not reserved for.
const MAX_PLAIN: u64 = 1 << 30;
/// The largest zstd window a vintage may ask for.
const MAX_WINDOW: u64 = 64 << 20;
/// How many times a week is read and merged again after another machine
/// replaced its vintage first.
const MERGE_TRIES: usize = 3;

/// The ISO 8601 week a moment falls in, `2026-W40`: weeks start on Monday,
/// and week 1 is the one holding the year's first Thursday, so the days
/// around New Year can belong to the year before or after.
pub fn iso_week(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    // 1970-01-01 was a Thursday; Monday is 1.
    let weekday = (days + 3).rem_euclid(7) + 1;
    let thursday = days - (weekday - 1) + 3;
    let (year, _, _) = civil_from_days(thursday);
    let week = (thursday - days_from_civil(year, 1, 1)) / 7 + 1;
    format!("{year:04}-W{week:02}")
}

/// Howard Hinnant's civil calendar from days since the epoch.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// One turn as the stub keeps it: enough to price it as krowk.db did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StubTurn {
    pub status: String,
    pub provider: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_micros: Option<i64>,
}

/// What an archived session keeps on the machine (R-VINT-3): the tiny
/// index row `krowk sessions` lists and search finds, and the week whose
/// vintage holds the rest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stub {
    pub week: String,
    /// The slug of the vintage it went into, for finding it again should
    /// the week's vintage be replaced by one that lost it.
    #[serde(default)]
    pub vintage: String,
    pub title: String,
    /// The first thing asked in it, longer than the title.
    pub summary: String,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub created_ms: i64,
    pub last_active_ms: i64,
    pub archived_ms: i64,
    pub provider: String,
    pub model: String,
    /// Every model it ran on, in the order first used.
    pub models: Vec<String>,
    pub turns: Vec<StubTurn>,
}

impl Stub {
    /// The stub a log leaves: its projection into krowk.db, kept small.
    pub fn of(events: &[LogEvent], week: &str, now_ms: i64) -> Option<Stub> {
        let th = crate::project::thread(events, &mut krowk_import::ReadResult::default())?;
        let parent = match &events.first()?.body {
            crate::protocol::LogBody::SessionStarted { parent_session_id, .. } => parent_session_id.clone(),
            _ => None,
        };
        let summary = th
            .messages
            .iter()
            .filter(|m| m.role == krowk_store::Role::User)
            .flat_map(|m| m.parts.iter().filter(|p| p.kind == "text"))
            .map(|p| serde_json::from_str::<serde_json::Value>(&p.data).ok().and_then(|v| v.get("text")?.as_str().map(str::to_owned)).unwrap_or_default())
            .find(|t| !t.trim().is_empty())
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(280).collect())
            .unwrap_or_default();
        let mut models: Vec<String> = Vec::new();
        let turns = th
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let answer = th.messages.iter().rev().find(|m| m.turn_seq == Some(i as i64) && m.role == krowk_store::Role::Assistant && !m.model.is_empty());
                let (provider, model) = answer.map_or((th.session.provider.clone(), th.session.model.clone()), |m| (m.provider.clone(), m.model.clone()));
                if !model.is_empty() && !models.contains(&model) {
                    models.push(model.clone());
                }
                StubTurn {
                    status: t.status.clone(),
                    provider,
                    model,
                    input: t.cost_input,
                    output: t.cost_output,
                    total: t.cost_total,
                    cache_read: t.cost_cache_read,
                    cache_write: t.cost_cache_write,
                    reasoning: t.cost_reasoning,
                    usd_micros: t.cost_usd_micros,
                }
            })
            .collect();
        Some(Stub {
            week: week.to_owned(),
            vintage: String::new(),
            title: krowk_store::title_for(&th.messages),
            summary,
            directory: th.session.directory.clone(),
            parent_session_id: parent,
            created_ms: events.iter().map(|e| e.time_ms).min().unwrap_or_default(),
            last_active_ms: last_active(events),
            archived_ms: now_ms,
            provider: th.session.provider.clone(),
            model: th.session.model.clone(),
            models,
            turns,
        })
    }
}

fn last_active(events: &[LogEvent]) -> i64 {
    events.iter().map(|e| e.time_ms).max().unwrap_or_default()
}

/// The stub in a session's directory, when it is archived.
pub fn read_stub(sessions: &Path, id: &str) -> Option<Stub> {
    if !log::valid_id(id) {
        return None;
    }
    let text = std::fs::read(sessions.join(id).join(STUB_FILE)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Whether a session was archived and has not been restored.
pub fn is_archived(sessions: &Path, id: &str) -> bool {
    log::valid_id(id) && sessions.join(id).join(STUB_FILE).is_file() && !sessions.join(id).join(EVENTS_FILE).is_file()
}

/// Every archived session, as (id, stub path).
pub fn list_archived(sessions: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(sessions) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            (is_archived(sessions, &id)).then(|| (id, e.path().join(STUB_FILE)))
        })
        .collect();
    out.sort();
    out
}

/// Pins or unpins a session: a pinned one is never archived (R-VINT-2).
pub fn pin(sessions: &Path, id: &str, pinned: bool) -> Result<(), String> {
    let dir = sessions.join(id);
    if !log::valid_id(id) || !dir.is_dir() {
        return Err(format!("no krowk session {id:?} in {}", sessions.display()));
    }
    let marker = dir.join(PIN_FILE);
    let r = if pinned { std::fs::write(&marker, b"") } else { std::fs::remove_file(&marker).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) };
    r.map_err(|e| format!("{} {}: {e}", if pinned { "write" } else { "remove" }, marker.display()))
}

pub fn is_pinned(sessions: &Path, id: &str) -> bool {
    sessions.join(id).join(PIN_FILE).exists()
}

/// Whether the weekly job is due: never run, or last run a week ago.
pub fn due(sessions: &Path, now_ms: i64) -> bool {
    let last: i64 = std::fs::read_to_string(sessions.join(STAMP_FILE)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    now_ms - last >= WEEK_MS
}

fn stamp(sessions: &Path, now_ms: i64) -> Result<(), String> {
    std::fs::write(sessions.join(STAMP_FILE), now_ms.to_string()).map_err(|e| format!("write {}: {e}", sessions.join(STAMP_FILE).display()))
}

/// One session going into a vintage. It holds its log's lock from being
/// chosen until its files are gone, so nothing appends to it in between.
struct Candidate {
    id: String,
    dir: PathBuf,
    week: String,
    stub: Stub,
    events: String,
    context: String,
    _lock: std::fs::File,
}

/// The sessions idle for longer than `idle_ms` at `now_ms`, never a pinned
/// one, a subagent of a pinned one, or one another krowk holds open.
fn candidates(sessions: &Path, now_ms: i64, idle_ms: i64) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (id, path) in log::list(sessions).unwrap_or_default() {
        let dir = sessions.join(&id);
        if is_pinned(sessions, &id) {
            continue;
        }
        let Ok(events) = log::read_events(&path) else { continue };
        let last = last_active(&events);
        if last == 0 || now_ms - last <= idle_ms {
            continue;
        }
        let week = iso_week(last);
        let Some(stub) = Stub::of(&events, &week, now_ms) else { continue };
        if stub.parent_session_id.as_deref().is_some_and(|p| log::valid_id(p) && is_pinned(sessions, p)) {
            continue;
        }
        let Ok(lock) = std::fs::OpenOptions::new().append(true).open(&path) else { continue };
        if lock.try_lock().is_err() {
            continue;
        }
        let Ok(ev) = std::fs::read_to_string(&path) else { continue };
        // Only a missing context is an empty one. One that cannot be read
        // would go into the vintage as nothing and then be deleted, so the
        // session is left where it is instead.
        let context = match std::fs::read_to_string(dir.join(CONTEXT_FILE)) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => continue,
        };
        out.push(Candidate { id, dir, week, stub, events: ev, context, _lock: lock });
    }
    out
}

/// The format of a vintage's line this krowk writes and reads.
pub const LINE_V1: u32 = 1;

/// One session inside a vintage: its log files, verbatim. A merge carries
/// every line it read into the vintage it writes, so fields a later krowk
/// added are kept as they came (`rest`). A line of a later format is
/// refused whole rather than merged down into this one's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    #[serde(default = "line_v1")]
    pub v: u32,
    pub id: String,
    pub events: String,
    pub context: String,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

fn line_v1() -> u32 {
    LINE_V1
}

impl Line {
    pub fn new(id: &str, events: String, context: String) -> Line {
        Line { v: LINE_V1, id: id.to_owned(), events, context, rest: serde_json::Map::new() }
    }
}

/// A week's sessions as the vintage's plaintext: JSONL, zstd-compressed.
pub fn pack(lines: &BTreeMap<String, Line>) -> Vec<u8> {
    let mut jsonl = Vec::new();
    for l in lines.values() {
        jsonl.extend(serde_json::to_vec(l).expect("a line serializes"));
        jsonl.push(b'\n');
    }
    ruzstd::encoding::compress_to_vec(&jsonl[..], ruzstd::encoding::CompressionLevel::Fastest)
}

pub fn unpack(packed: &[u8]) -> Result<BTreeMap<String, Line>, String> {
    use std::io::{BufRead, Read};
    let d = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(packed, MAX_WINDOW).map_err(|e| format!("the vintage is not zstd, or asks for a window past {MAX_WINDOW} bytes: {e}"))?;
    let mut jsonl = Vec::new();
    d.take(MAX_PLAIN + 1).read_to_end(&mut jsonl).map_err(|e| format!("the vintage does not decompress: {e}"))?;
    if jsonl.len() as u64 > MAX_PLAIN {
        return Err(format!("the vintage decompresses past {MAX_PLAIN} bytes"));
    }
    let mut out = BTreeMap::new();
    for line in jsonl.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let l: Line = serde_json::from_str(&line).map_err(|e| format!("the vintage holds a line this krowk does not read: {e}"))?;
        if l.v > LINE_V1 {
            return Err(format!("the vintage holds session {} in format {}, newer than this krowk reads — upgrade krowk", l.id, l.v));
        }
        if !log::valid_id(&l.id) {
            return Err(format!("the vintage names a session {:?} that is no krowk session id", l.id));
        }
        out.insert(l.id.clone(), l);
    }
    Ok(out)
}

/// A week's sessions, by id.
pub type Week = BTreeMap<String, Line>;

/// The week's vintage as the registry holds it, opened: its slug, to name
/// as replaced, and its sessions.
fn fetch(client: &Client, account: &AccountKey, week: &str) -> Result<Option<(String, Week)>, String> {
    let Some(v) = client.week_vintage(week).map_err(|e| e.fix())? else { return Ok(None) };
    let sealed = client.read_vintage(&v).map_err(|e| e.fix())?;
    let packed = e2e::open_vintage(&sealed, week, account).map_err(|e| e.to_string())?;
    Ok(Some((v.slug, unpack(&packed)?)))
}

/// A session archived by a run: its id and the week it went into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub id: String,
    pub week: String,
}

/// What a run did: the sessions it archived, and why it stopped early
/// when it did. The weeks before a failure stay archived and are reported,
/// so the caller still drops their bodies from krowk.db.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    pub archived: Vec<Archived>,
    pub failed: Option<String>,
}

/// Archives every native session idle for more than `idle_days` at
/// `now_ms`, week by week, and stamps a run that finished. A week that
/// fails leaves its sessions exactly as they were — no stub, the log files
/// in place — and ends the run.
pub fn archive(client: &Client, account: &AccountKey, sessions: &Path, now_ms: i64, idle_days: u64) -> Run {
    let mut weeks: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for c in candidates(sessions, now_ms, idle_days as i64 * DAY_MS) {
        weeks.entry(c.week.clone()).or_default().push(c);
    }
    let mut run = Run::default();
    for (week, group) in weeks {
        let slug = match store_week(client, account, &week, &group) {
            Ok(slug) => slug,
            Err(e) => {
                run.failed = Some(e);
                return run;
            }
        };
        for mut c in group {
            c.stub.vintage.clone_from(&slug);
            if let Err(e) = leave_stub(&c) {
                run.failed = Some(e);
                return run;
            }
            run.archived.push(Archived { id: c.id, week: week.clone() });
        }
    }
    let _ = std::fs::create_dir_all(sessions);
    if let Err(e) = stamp(sessions, now_ms) {
        run.failed = Some(e);
    }
    run
}

/// Writes the week's vintage with `group` merged into whatever it already
/// holds, reading again when another machine replaced it meanwhile.
fn store_week(client: &Client, account: &AccountKey, week: &str, group: &[Candidate]) -> Result<String, String> {
    for _ in 0..MERGE_TRIES {
        let (replaces, mut lines) = match fetch(client, account, week)? {
            Some((slug, lines)) => (Some(slug), lines),
            None => (None, BTreeMap::new()),
        };
        for c in group {
            lines.insert(c.id.clone(), Line::new(&c.id, c.events.clone(), c.context.clone()));
        }
        let sealed = e2e::seal_vintage(account, week, &pack(&lines));
        match client.put_vintage(week, &sealed, replaces.as_deref()) {
            Ok(v) => return Ok(v.slug),
            Err(e) if e.code() == "vintage_conflict" => continue,
            Err(e) => return Err(format!("the vintage for {week} was not stored, so its sessions stay on this machine: {}", e.fix())),
        }
    }
    Err(format!("the vintage for {week} kept being replaced by another machine; its sessions stay here until the next run"))
}

/// The registry holds the session: the stub goes in, then the log goes.
fn leave_stub(c: &Candidate) -> Result<(), String> {
    let stub = serde_json::to_vec_pretty(&c.stub).map_err(|e| e.to_string())?;
    write_private(&c.dir.join(STUB_FILE), &stub)?;
    for f in [EVENTS_FILE, CONTEXT_FILE] {
        match std::fs::remove_file(c.dir.join(f)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("remove {}: {e}", c.dir.join(f).display())),
            _ => {}
        }
    }
    Ok(())
}

/// Brings an archived session back (R-VINT-4): its week's vintage fetched,
/// checked and opened, its two files written back, the stub removed.
pub fn restore(client: &Client, account: &AccountKey, sessions: &Path, id: &str) -> Result<(), String> {
    let stub = read_stub(sessions, id).ok_or_else(|| format!("session {id} is not archived on this machine"))?;
    let (_, lines) = fetch(client, account, &stub.week)?.ok_or_else(|| format!("the registry holds no vintage for {}, where session {id} was archived", stub.week))?;
    let line = lines.get(id).ok_or_else(|| format!("the vintage for {} does not hold session {id} — the registry may have served an older one", stub.week))?;
    let dir = sessions.join(id);
    write_private(&dir.join(CONTEXT_FILE), line.context.as_bytes())?;
    write_private(&dir.join(EVENTS_FILE), line.events.as_bytes())?;
    std::fs::remove_file(dir.join(STUB_FILE)).map_err(|e| format!("remove {}: {e}", dir.join(STUB_FILE).display()))
}

/// Writes a file whole, 0600, through a temporary beside it and a rename,
/// so a crash leaves the old file or the new one and never half of one.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = DAY_MS;

    fn at(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d) * DAY
    }

    /// ISO weeks, New Year's edges included: a vintage is named for the
    /// week its sessions were last active in (R-VINT-1).
    #[test]
    fn r_vint_1_sessions_are_grouped_by_iso_week() {
        assert_eq!(iso_week(at(2026, 9, 29)), "2026-W40");
        assert_eq!(iso_week(at(2026, 9, 28)), "2026-W40", "Monday starts the week");
        assert_eq!(iso_week(at(2026, 9, 27) + DAY - 1), "2026-W39", "Sunday ends the one before");
        assert_eq!(iso_week(at(2021, 1, 1)), "2020-W53");
        assert_eq!(iso_week(at(2024, 12, 30)), "2025-W01");
        assert_eq!(iso_week(0), "1970-W01");
    }

    /// The plaintext is zstd JSONL, one line per session, and reads back
    /// as it went in; a line naming no session id is refused.
    #[test]
    fn r_vint_1_a_week_packs_as_zstd_jsonl_and_unpacks_whole() {
        let id = "0192f1e2-3a4b-7c5d-8e6f-0123456789ab".to_string();
        let lines = BTreeMap::from([(id.clone(), Line::new(&id, "{\"a\":1}\n".repeat(200), "{}\n".into()))]);
        let packed = pack(&lines);
        assert_eq!(&packed[..4], &[0x28, 0xb5, 0x2f, 0xfd], "a zstd frame");
        assert!(packed.len() < 200 * 8, "compressed");
        assert_eq!(unpack(&packed).unwrap(), lines);
        let bad = ruzstd::encoding::compress_to_vec(&b"{\"id\":\"../x\",\"events\":\"\",\"context\":\"\"}\n"[..], ruzstd::encoding::CompressionLevel::Fastest);
        assert!(unpack(&bad).is_err());
    }

    fn packed(jsonl: &str) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(jsonl.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// A merge by this krowk keeps what a later one wrote into a line it
    /// does not know, and a line of a later format is refused, never
    /// merged down.
    #[test]
    fn r_vint_1_a_merge_keeps_a_newer_krowks_fields_and_refuses_a_newer_format() {
        let theirs = "0192f1e2-3a4b-7c5d-8e6f-0123456789ab";
        let ours = "0192f1e2-3a4b-7c5d-8e6f-0123456789ac";
        let mut week = unpack(&packed(&format!("{{\"v\":1,\"id\":\"{theirs}\",\"events\":\"e\\n\",\"context\":\"\",\"forks\":[{{\"head\":\"x\"}}]}}\n"))).unwrap();
        week.insert(ours.into(), Line::new(ours, "mine\n".into(), String::new()));
        let again = unpack(&pack(&week)).unwrap();
        assert_eq!(again[theirs].rest["forks"], serde_json::json!([{"head": "x"}]), "kept through the merge");
        assert_eq!(again[ours].v, LINE_V1);
        let newer = unpack(&packed(&format!("{{\"v\":2,\"id\":\"{theirs}\",\"events\":\"\",\"context\":\"\"}}\n"))).unwrap_err();
        assert!(newer.contains("upgrade krowk"), "{newer}");
    }

    fn idle_session(sessions: &Path, days: i64) -> String {
        let (mut l, root) = log::SessionLog::create(sessions, sessions, "test").unwrap();
        l.append(crate::protocol::LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: crate::protocol::Item::user("hi") }).unwrap();
        drop(l);
        let path = sessions.join(&root.session_id).join(EVENTS_FILE);
        let aged: String = log::read_events(&path)
            .unwrap()
            .into_iter()
            .map(|mut e| {
                e.time_ms -= days * DAY;
                serde_json::to_string(&e).unwrap() + "\n"
            })
            .collect();
        std::fs::write(&path, aged).unwrap();
        root.session_id
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("krowk-vintage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A context that exists but cannot be read is not archived as empty:
    /// the session is skipped and stays whole (R-VINT-2).
    #[test]
    fn r_vint_2_a_session_whose_context_cannot_be_read_is_not_archived() {
        let dir = scratch("unreadable");
        let id = idle_session(&dir, 20);
        let context = dir.join(&id).join(CONTEXT_FILE);
        let _ = std::fs::remove_file(&context);
        std::fs::create_dir(&context).unwrap();
        let now = krowk_store::now_ms();
        assert!(candidates(&dir, now, 14 * DAY).is_empty());
        std::fs::remove_dir(&context).unwrap();
        assert_eq!(candidates(&dir, now, 14 * DAY).len(), 1, "a missing context is an empty one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A vintage the registry never stores leaves the session exactly as
    /// it was: its log files in place, no stub, no stamp (R-VINT-1).
    #[test]
    fn r_vint_1_a_failed_store_leaves_the_session_on_the_machine() {
        let dir = scratch("failed");
        let id = idle_session(&dir, 20);
        let before = std::fs::read(dir.join(&id).join(EVENTS_FILE)).unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", dead.local_addr().unwrap());
        drop(dead);
        let client = Client::new(&url, "krowk_sk_dead_000000000000000000000000");
        let run = archive(&client, &AccountKey::generate(), &dir, krowk_store::now_ms(), 14);
        assert!(run.archived.is_empty() && run.failed.is_some(), "{run:?}");
        assert_eq!(std::fs::read(dir.join(&id).join(EVENTS_FILE)).unwrap(), before);
        assert!(!dir.join(&id).join(STUB_FILE).exists());
        assert!(due(&dir, krowk_store::now_ms()), "an unfinished run is not stamped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
