//! gruntas_status, gruntas_changes and gruntas_ping (GA03, R-GOD-2,
//! R-GOD-3, R-GOD-5): the Status layer's tree from every board, compact and
//! paged; each machine's changes through a `changes` host command, the
//! silent ones listed as unknown; the Status layer's ping.
//!
//! `since` (D4) is each machine's own clock: gruntas_status gives every
//! board's publish time, gruntas_changes each answer's time, as
//! `<device>=<time>` pairs joined by commas, so no two clocks are compared.

use super::super::{arguments, Outcome};
use crate::cli::{Asked, Machine};
use gruntas_api::fail;
use gruntas_client::host_command::{Change, HostAnswer, HostCommand};
use gruntas_client::status::TaskState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Rows per page of gruntas_status (D1).
pub(super) const ROWS: usize = 200;

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusArgs {
    project: Option<String>,
    page: Option<usize>,
}

pub(super) fn status(args: &Value) -> Outcome {
    let a: StatusArgs = arguments(args, "gruntas_status takes optionally a project and a page")?;
    let only = a.project.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(|p| gruntas_client::presence::normalise_origin(p).unwrap_or_else(|| p.to_string()));
    let (machines, unopened) = crate::cli::boards()?;
    let mut out = told_status(&machines, only.as_deref(), a.page.unwrap_or(1), jiff::Timestamp::now());
    for (_, name, e) in &unopened {
        out.0.push_str(&format!("\n{}'s status board does not open here: {}", one_line(name), one_line(e)));
    }
    out.1["unopened"] = json!(unopened.iter().map(|(d, n, e)| json!({"device": d, "name": n, "error": e})).collect::<Vec<Value>>());
    Ok(out)
}

/// One row of the compact tree: its project, its machine's line, its own.
type Row = (String, String, String);

/// `machines`' tree, `only` the project asked, page `page` of it as the
/// tool says it, as of `now`.
fn told_status(machines: &[Machine], only: Option<&str>, page: usize, now: jiff::Timestamp) -> (String, Value) {
    let (tree, _) = crate::cli::tree(machines, only, now);
    let mut rows: Vec<Row> = Vec::new();
    for p in &tree {
        let project = one_line(p["project"].as_str().unwrap_or_default());
        for m in p["machines"].as_array().into_iter().flatten() {
            let machine = format!("  {}  {}", one_line(m["name"].as_str().unwrap_or_default()), m["health"].as_str().unwrap_or_default());
            for s in m["sessions"].as_array().into_iter().flatten() {
                session_rows(s, 2, &mut |line| rows.push((project.clone(), machine.clone(), line)));
            }
        }
    }
    let pages = rows.len().div_ceil(ROWS).max(1);
    let page = page.clamp(1, pages);
    let mut lines = Vec::new();
    let mut under: Option<(&str, &str)> = None;
    for (project, machine, line) in rows.iter().skip((page - 1) * ROWS).take(ROWS) {
        if under.is_none_or(|u| u.0 != project) {
            lines.push(project.clone());
            under = None;
        }
        if under.is_none_or(|u| u.1 != machine) {
            lines.push(machine.clone());
        }
        under = Some((project, machine));
        lines.push(line.clone());
    }
    if rows.is_empty() {
        lines.push(match only {
            Some(p) => format!("No agents on {} on any of your machines.", one_line(p)),
            None => "No agents on any of your machines yet.".into(),
        });
    }
    if pages > 1 {
        lines.push(format!("Page {page} of {pages} ({} sessions); pass page {} for more.", rows.len(), if page < pages { page + 1 } else { 1 }));
    }
    let since = token(machines.iter().map(|m| (m.device.as_str(), m.board.published_at.as_str())));
    lines.push(format!("since: {since}"));
    (lines.join("\n"), json!({ "projects": tree, "page": page, "pages": pages, "since": since }))
}

/// `s` and its children, depth first, one line each: id, state, health,
/// quiet for and summary.
fn session_rows(s: &Value, depth: usize, push: &mut dyn FnMut(String)) {
    let state = serde_json::from_value::<TaskState>(s["state"].clone()).map_or_else(|_| "unspecified".into(), crate::cli::state_word);
    let quiet = s["quietForSeconds"].as_i64().map_or_else(|| "quiet ?".into(), |q| format!("quiet {}", crate::cli::span(jiff::SignedDuration::from_secs(q))));
    let summary = s["summary"].as_str().filter(|m| !m.is_empty()).map(|m| format!("  {}", one_line(m))).unwrap_or_default();
    push(format!("{:indent$}{}  {state}  {}  {quiet}{summary}", "", one_line(s["id"].as_str().unwrap_or_default()), s["health"].as_str().unwrap_or_default(), indent = depth * 2));
    for c in s["children"].as_array().into_iter().flatten() {
        session_rows(c, depth + 1, push);
    }
}

/// Text from a board or a host, on one line: a newline in it must not read
/// as a row of its own.
fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Each machine's own time, as `since` carries it.
fn token<'a>(times: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    times.map(|(d, t)| format!("{d}={t}")).collect::<Vec<_>>().join(",")
}

/// A `since` read: one time for every machine, or each machine's own; a
/// machine a token leaves out is asked from the oldest time in it.
fn since_for(since: &str) -> Result<(BTreeMap<String, String>, String), String> {
    let since = since.trim();
    if let Ok(t) = since.parse::<jiff::Timestamp>() {
        return Ok((BTreeMap::new(), t.to_string()));
    }
    let mut each = BTreeMap::new();
    let mut oldest: Option<jiff::Timestamp> = None;
    for pair in since.split(',') {
        let (d, t) = pair.split_once('=').ok_or("since is neither a time nor the since a gruntas tool returned")?;
        let t: jiff::Timestamp = t.trim().parse().map_err(|_| format!("{:?} in since is not a time", t.escape_debug().to_string()))?;
        oldest = Some(oldest.map_or(t, |o| o.min(t)));
        each.insert(d.trim().to_ascii_lowercase(), t.to_string());
    }
    Ok((each, oldest.map(|t| t.to_string()).unwrap_or_default()))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChangesArgs {
    since: String,
}

pub(super) fn changes(args: &Value) -> Outcome {
    let a: ChangesArgs = arguments(args, "gruntas_changes takes since: the since a gruntas_status or gruntas_changes returned, or an RFC 3339 time")?;
    let (each, rest) = since_for(&a.since).map_err(|why| fail("bad_arguments", why))?;
    let asked_since = |device: &str| each.get(&device.to_ascii_lowercase()).cloned().unwrap_or_else(|| rest.clone());
    let asked = crate::cli::ask_every(|device| HostCommand::Changes { since: asked_since(device) })?;
    Ok(told_changes(asked, &asked_since))
}

/// Each machine's answer to `changes` as the tool says it: its changes,
/// oldest first, or the machine listed as unknown with why; and the next
/// `since`, an unknown machine's left where it was.
fn told_changes(mut asked: Vec<Asked>, since: &dyn Fn(&str) -> String) -> (String, Value) {
    asked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let (mut lines, mut unknown, mut answered, mut next) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (name, device, answer) in asked {
        let name = one_line(&name);
        let why = match answer {
            Ok(HostAnswer::Changes(c)) => {
                lines.push(format!("{name}: {}since {}", if c.changes.is_empty() { "nothing " } else { "" }, c.since));
                lines.extend(c.changes.iter().map(change_line));
                next.push((device.clone(), c.at.clone()));
                answered.push(json!({ "name": name, "device": device, "since": c.since, "at": c.at, "changes": c.changes }));
                continue;
            }
            Ok(other) => other.refusal_line(&name).unwrap_or_else(|| format!("{name} answered as another command")),
            Err((_, why)) => why,
        };
        next.push((device.clone(), since(&device)));
        unknown.push(json!({ "name": name, "device": device, "why": why }));
    }
    for u in &unknown {
        lines.push(format!("{}: unknown — {}", u["name"].as_str().unwrap_or_default(), one_line(u["why"].as_str().unwrap_or_default())));
    }
    if answered.is_empty() && unknown.is_empty() {
        lines.push("No machine of yours publishes a status board yet.".into());
    }
    let next = token(next.iter().map(|(d, t)| (d.as_str(), t.as_str())));
    lines.push(format!("since: {next}"));
    (lines.join("\n"), json!({ "machines": answered, "unknown": unknown, "since": next }))
}

fn change_line(c: &Change) -> String {
    let state = c.state.map(crate::cli::state_word).unwrap_or_default();
    let summary = c.summary.as_deref().map(|s| format!("  reported: {}", one_line(s))).unwrap_or_default();
    format!("  {}  {}  {state}{summary}", c.at, one_line(&c.session))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PingArgs {
    target: String,
}

pub(super) fn ping(args: &Value) -> Outcome {
    let a: PingArgs = arguments(args, "gruntas_ping takes a target: a machine's name or a session's id")?;
    if a.target.trim().is_empty() {
        return Err(fail("bad_arguments", "gruntas_ping needs a target: a machine's name, as gruntas_status lists it, or a session's id"));
    }
    let data = crate::cli::ping_data(a.target.trim())?;
    let machine = one_line(data["machine"].as_str().unwrap_or_default());
    let took = data["answeredInSeconds"].as_f64().unwrap_or_default();
    let text = match data.get("session").filter(|s| !s.is_null()) {
        Some(s) => {
            let quiet = s["quietForSeconds"].as_i64().map_or_else(|| "no activity known".into(), |q| format!("quiet for {}", crate::cli::span(jiff::SignedDuration::from_secs(q))));
            format!("{} on {machine}: answered in {took:.1} s, {}, {quiet}", one_line(s["id"].as_str().unwrap_or_default()), s["health"].as_str().unwrap_or("UNKNOWN"))
        }
        None => format!("{machine}: answered in {took:.1} s"),
    };
    Ok((text, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::host_command::ChangesSince;
    use gruntas_client::status::{Board, Field, Health, Message, Task, TaskMetadata, TaskStatus};

    const AT: &str = "2026-11-24T10:00:00Z";

    fn task(id: String, project: &str) -> Task {
        let mut metadata = TaskMetadata::default();
        metadata.gruntas.project = Some(Field::harness(project.to_string()));
        metadata.gruntas.last_activity = Some(Field::harness(AT.to_string()));
        metadata.gruntas.health = Some(Field::harness(Health::Serving));
        Task { id, status: TaskStatus { state: TaskState::Working, message: Some(Message::text("m", "building")), timestamp: AT.into() }, history: Vec::new(), metadata }
    }

    fn machine(name: &str, tasks: Vec<Task>) -> Machine {
        let board = Board { device: format!("{name}-device"), published_at: AT.into(), sessions: tasks.iter().map(|t| t.id.clone()).collect(), tasks };
        Machine { device: board.device.clone(), name: name.into(), health: Health::Serving, board, received_at: AT.parse().unwrap() }
    }

    /// R-GOD-2: one line per session under its project and machine, one
    /// project when asked, paged beyond 200 rows with the headers again.
    #[test]
    fn ga03_status_filters_by_project_and_pages() {
        let many = (0..250).map(|i| task(format!("app-{i:03}"), "github.com/acme/app")).collect();
        let machines = [machine("laptop", many), machine("desk", vec![task("site-1".into(), "github.com/acme/site")])];
        let now = "2026-11-24T10:03:00Z".parse().unwrap();
        let (text, data) = told_status(&machines, Some("github.com/acme/site"), 1, now);
        assert_eq!(text.lines().collect::<Vec<_>>(), ["github.com/acme/site", "  desk  SERVING", "    site-1  working  SERVING  quiet 3m  building", "since: laptop-device=2026-11-24T10:00:00Z,desk-device=2026-11-24T10:00:00Z"]);
        assert_eq!(data["pages"], 1);
        let (first, data) = told_status(&machines, None, 1, now);
        assert_eq!((data["pages"].as_u64(), first.lines().filter(|l| l.starts_with("    ")).count()), (Some(2), ROWS));
        assert!(first.contains("Page 1 of 2 (251 sessions); pass page 2 for more."), "{first}");
        let (second, _) = told_status(&machines, None, 2, now);
        let second: Vec<&str> = second.lines().collect();
        assert_eq!(second[..3], ["github.com/acme/app", "  laptop  SERVING", "    app-200  working  SERVING  quiet 3m  building"]);
        assert_eq!(second.iter().filter(|l| l.starts_with("    ")).count(), 51);
    }

    /// R-GOD-3: each answering machine's changes, oldest first; a silent
    /// machine and an outdated one listed as unknown, their since kept.
    #[test]
    fn ga03_changes_list_a_silent_machine_as_unknown() {
        let (each, rest) = since_for("aa=2026-11-24T09:00:00Z,bb=2026-11-24T08:00:00Z").unwrap();
        assert_eq!((each["aa"].as_str(), rest.as_str()), ("2026-11-24T09:00:00Z", "2026-11-24T08:00:00Z"));
        assert!(since_for("yesterday").is_err());
        let change = Change { session: "s1".into(), at: "2026-11-24T09:30:00Z".into(), state: Some(TaskState::Completed), summary: Some("built it".into()) };
        let asked: Vec<Asked> = vec![
            ("laptop".into(), "aa".into(), Ok(HostAnswer::Changes(ChangesSince { since: "2026-11-24T09:00:00Z".into(), at: "2026-11-24T10:00:00Z".into(), changes: vec![change] }))),
            ("desk".into(), "bb".into(), Err(("host_silent".into(), "desk didn't answer within 30 s".into()))),
            ("old".into(), "cc".into(), Err(("host_outdated".into(), "old: that machine needs updating — update gruntas there".into()))),
        ];
        let since = |d: &str| if d == "aa" { "2026-11-24T09:00:00Z".to_string() } else { rest.clone() };
        let (text, data) = told_changes(asked, &since);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..2], ["laptop: since 2026-11-24T09:00:00Z", "  2026-11-24T09:30:00Z  s1  completed  reported: built it"]);
        assert_eq!(lines[2..4], ["desk: unknown — desk didn't answer within 30 s", "old: unknown — old: that machine needs updating — update gruntas there"]);
        assert_eq!(data["unknown"].as_array().unwrap().len(), 2);
        assert_eq!(data["since"], "bb=2026-11-24T08:00:00Z,aa=2026-11-24T10:00:00Z,cc=2026-11-24T08:00:00Z");
    }
}
