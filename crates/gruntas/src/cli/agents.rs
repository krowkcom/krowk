//! `gruntas status [project]` (once `gruntas agents`, still routed) and `gruntas ping <machine|session>` (R-STA-7,
//! D5, D10): the status layer, read from this device.
//!
//! - `agents`: every machine's sealed status board (`sync::boards::read`),
//!   as a tree by project, machine and parent session; each session with
//!   its A2A state, its health, how long it has been quiet and its summary.
//!   A machine is `SERVING` while its presence is live and `UNKNOWN` once it
//!   lapsed (D4). "Quiet for" is the board's publish time less the session's
//!   last activity, plus the time since this device received the board, so
//!   no two machines' clocks are compared (D6). `--json` is the same tree.
//! - `ping`: a `ping` host command to the machine's daemon, which answers
//!   with an empty `pong` (MCP's `ping`): "answered in N s", which includes
//!   the target's presence refresh (up to 10 s), so it is a liveness check,
//!   not a latency. A session is found on the boards, and its machine
//!   pinged; the answer adds its health and last activity. A machine whose
//!   presence lapsed is `UNKNOWN`, said at once with nothing sent.

use super::synced::{command_id, find_device, keys, signed, unreachable, Keys};
use super::sync::printable;
use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Client, Error};
use gruntas_client::host_command::{self, HostAnswer, HostCommand, SessionPing};
use gruntas_client::status::{self, Board, Health, Task, TaskState};
use gruntas_harness::sync::boards;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// How long `ping` waits for the answer: a host sees a command within one
/// 10 s presence refresh.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const ANSWER_EVERY: std::time::Duration = std::time::Duration::from_millis(250);
/// The project a session with none is listed under.
const NO_PROJECT: &str = "(no project)";

/// One machine's board as read: the machine's name on the device list, its
/// health, and the board with when it was received.
pub(crate) struct Machine {
    pub(crate) device: String,
    pub(crate) name: String,
    pub(crate) health: Health,
    pub(crate) board: Board,
    pub(crate) received_at: jiff::Timestamp,
}

/// A device whose board did not open: its id, its name and why.
pub(crate) type Unopened = (String, String, String);

/// The boards, read and opened; the devices whose board did not open.
fn read(ctx: &Ctx, k: &Keys, what: &str) -> Result<(Client, Vec<Machine>, Vec<Unopened>), Error> {
    let api = signed(ctx, k, what)?;
    let mut machines = Vec::new();
    let mut unopened = Vec::new();
    for r in boards::read(&api, &k.user, &k.chain)? {
        let name = name_of(k, &r.device);
        match r.board {
            Ok(board) => machines.push(Machine { health: if r.online { Health::Serving } else { Health::Unknown }, device: r.device, name, board, received_at: r.received_at }),
            Err(e) => unopened.push((r.device, name, e)),
        }
    }
    machines.sort_by(|a, b| a.name.cmp(&b.name).then(a.device.cmp(&b.device)));
    Ok((api, machines, unopened))
}

/// A device's name on the verified list, else its id.
fn name_of(k: &Keys, device: &str) -> String {
    k.chain.devices().iter().find(|d| d.id().to_string().eq_ignore_ascii_case(device)).map_or_else(|| device.to_string(), |d| printable(&d.name))
}

/// A2A's state as a person reads it: `TASK_STATE_INPUT_REQUIRED` is
/// `input-required`.
pub(crate) fn state_word(s: TaskState) -> String {
    let v = serde_json::to_value(s).ok();
    let wire = v.as_ref().and_then(Value::as_str).unwrap_or("TASK_STATE_UNSPECIFIED");
    wire.trim_start_matches("TASK_STATE_").to_ascii_lowercase().replace('_', "-")
}

fn health_word(h: Health) -> &'static str {
    match h {
        Health::Serving => "SERVING",
        Health::NotServing => "NOT_SERVING",
        Health::Unknown => "UNKNOWN",
    }
}

/// A span as "42s", "3m", "2h" or "1d".
pub(crate) fn span(d: jiff::SignedDuration) -> String {
    let s = d.as_secs().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn project_of(t: &Task) -> String {
    t.metadata.gruntas.project.as_ref().map_or_else(|| NO_PROJECT.to_string(), |p| p.value.clone())
}

/// `gruntas status [project]`.
pub(super) fn agents(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let only = match args {
        [] => None,
        [p] => Some(gruntas_client::presence::normalise_origin(p).unwrap_or_else(|| p.trim().to_string())),
        _ => return Err(fail("bad_args", "`gruntas status` takes at most one project, e.g. gruntas status github.com/acme/app")),
    };
    let k = keys(ctx)?;
    let (_, machines, unopened) = read(ctx, &k, "gruntas status")?;
    let (tree, n) = tree(&machines, only.as_deref(), jiff::Timestamp::now());
    let unopened_json: Vec<Value> = unopened.iter().map(|(d, n, e)| json!({"device": d, "name": n, "error": e})).collect();
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({"projects": tree, "unopened": unopened_json}), format!("{n} agents"));
    }
    let out = &mut *ctx.io.stdout;
    for p in &tree {
        let _ = writeln!(out, "{}", printable(p["project"].as_str().unwrap_or_default()));
        for m in p["machines"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "  {}  {}", printable(m["name"].as_str().unwrap_or_default()), m["health"].as_str().unwrap_or_default());
            for s in m["sessions"].as_array().into_iter().flatten() {
                print_session(out, s, 2);
            }
        }
    }
    if tree.is_empty() {
        let _ = match &only {
            Some(p) => writeln!(out, "no agents on {} on any of your machines", printable(p)),
            None => writeln!(out, "no agents on any of your machines yet — sessions show here once the gruntas plugin registers them"),
        };
    }
    for (_, name, e) in &unopened {
        let _ = writeln!(out, "{}'s status board does not open here: {}", printable(name), printable(e));
    }
    Ok(())
}

/// `machines`' sessions as `gruntas status --json` gives them: by project,
/// `only` that one when given, then machine, then parent session; and how
/// many sessions that is. "Quiet for" is as of `now`.
pub(crate) fn tree(machines: &[Machine], only: Option<&str>, now: jiff::Timestamp) -> (Vec<Value>, usize) {
    // Project → machine (by its place in `machines`) → its Tasks.
    let mut projects: BTreeMap<String, BTreeMap<usize, Vec<&Task>>> = BTreeMap::new();
    for (i, m) in machines.iter().enumerate() {
        for t in &m.board.tasks {
            let p = project_of(t);
            if only.is_none_or(|o| o == p) {
                projects.entry(p).or_default().entry(i).or_default().push(t);
            }
        }
    }
    let session = |m: &Machine, t: &Task| {
        let g = &t.metadata.gruntas;
        json!({
            "id": t.id,
            "state": t.status.state,
            "health": g.health.as_ref().map_or(Health::Unknown, |h| h.value),
            "quietForSeconds": status::quiet_for(&m.board, t, m.received_at, now).map(|q| q.as_secs()),
            "summary": t.status.message.as_ref().map(|s| s.summary()),
            "lastActivity": g.last_activity.as_ref().map(|l| &l.value),
            "harness": g.harness.as_ref().map(|h| &h.value),
        })
    };
    let tree: Vec<Value> = projects
        .iter()
        .map(|(p, by_machine)| {
            let ms: Vec<Value> = by_machine
                .iter()
                .map(|(&i, tasks)| {
                    let m = &machines[i];
                    let sessions: Vec<Value> = roots(tasks).into_iter().map(|t| with_children(t, tasks, &|t| session(m, t))).collect();
                    json!({"device": m.device, "name": m.name, "health": m.health, "publishedAt": m.board.published_at, "sessions": sessions})
                })
                .collect();
            json!({"project": p, "machines": ms})
        })
        .collect();
    let n: usize = projects.values().flat_map(|m| m.values()).map(Vec::len).sum();
    (tree, n)
}

/// The Tasks with no parent among `tasks`, in the board's order; then, so
/// that parents naming each other in a cycle still show, the first of
/// each such group no root reaches.
fn roots<'a>(tasks: &[&'a Task]) -> Vec<&'a Task> {
    let mut roots: Vec<&Task> = tasks.iter().copied().filter(|t| parent(t).is_none_or(|p| p == t.id || !tasks.iter().any(|o| o.id == p))).collect();
    let mut reached: Vec<&str> = Vec::new();
    let reach = |from: &'a Task, reached: &mut Vec<&'a str>| {
        let mut todo = vec![from];
        while let Some(t) = todo.pop() {
            if !reached.contains(&t.id.as_str()) {
                reached.push(&t.id);
                todo.extend(tasks.iter().copied().filter(|c| parent(c) == Some(t.id.as_str())));
            }
        }
    };
    for r in &roots {
        reach(r, &mut reached);
    }
    for t in tasks {
        if !reached.contains(&t.id.as_str()) {
            roots.push(t);
            reach(t, &mut reached);
        }
    }
    roots
}

fn parent(t: &Task) -> Option<&str> {
    t.metadata.gruntas.parent_session_id.as_ref().map(|p| p.value.as_str())
}

/// `t` as `row` makes it, with its children under `children`, depth first.
/// A cycle among parents is cut where it closes.
fn with_children(t: &Task, tasks: &[&Task], row: &dyn Fn(&Task) -> Value) -> Value {
    fn walk(t: &Task, tasks: &[&Task], row: &dyn Fn(&Task) -> Value, seen: &mut Vec<String>) -> Value {
        seen.push(t.id.clone());
        let mut v = row(t);
        let mut children = Vec::new();
        for c in tasks {
            if parent(c) == Some(t.id.as_str()) && !seen.contains(&c.id) {
                children.push(walk(c, tasks, row, seen));
            }
        }
        v["children"] = Value::Array(children);
        v
    }
    walk(t, tasks, row, &mut Vec::new())
}

fn print_session(out: &mut dyn std::io::Write, s: &Value, depth: usize) {
    let state = serde_json::from_value::<TaskState>(s["state"].clone()).map_or_else(|_| "unspecified".into(), state_word);
    let quiet = s["quietForSeconds"].as_i64().map_or_else(|| "quiet ?".to_string(), |q| format!("quiet {}", span(jiff::SignedDuration::from_secs(q))));
    let summary = s["summary"].as_str().map(|m| format!("  {}", printable(m))).unwrap_or_default();
    let _ = writeln!(out, "{:indent$}{}  {state}  {}  {quiet}{summary}", "", printable(s["id"].as_str().unwrap_or_default()), s["health"].as_str().unwrap_or_default(), indent = depth * 2);
    for c in s["children"].as_array().into_iter().flatten() {
        print_session(out, c, depth + 1);
    }
}

/// The machine whose board lists the session `target`, its id in full or 8
/// or more characters it starts with: the machine, the session's full id
/// and its device. `none` says what was looked for when nothing is found.
fn holder<'a>(machines: &'a [Machine], target: &str, none: &str) -> Result<(&'a Machine, &'a String, gruntas_client::e2e::DeviceId), Error> {
    let on = |exact: bool| -> Vec<(&Machine, &String)> { machines.iter().flat_map(|m| m.board.sessions.iter().filter(move |s| if exact { *s == target } else { target.len() >= 8 && s.starts_with(target) }).map(move |s| (m, s))).collect() };
    let mut found = on(true);
    if found.is_empty() {
        found = on(false);
    }
    // One session on two boards (moved, its old machine's board not yet
    // replaced): the machine that is up, then the newest board.
    let first = found.first().map(|(_, s)| s.as_str());
    if found.iter().all(|(_, s)| Some(s.as_str()) == first) {
        found.sort_by_key(|(m, _)| (m.health == Health::Serving, m.board.published_at.parse::<jiff::Timestamp>().ok()));
        found.drain(..found.len().saturating_sub(1));
    }
    let (m, s) = match found[..] {
        [one] => one,
        [] => return Err(fail("no_agent", format!("{none} '{}' — gruntas machines and gruntas status list them", printable(target)))),
        _ => return Err(fail("ambiguous_session", format!("'{}' names more than one session — give more of its id", printable(target)))),
    };
    let d = gruntas_client::e2e::DeviceId::parse(&m.device).ok_or_else(|| fail("bad_board", "a board names no device id"))?;
    Ok((m, s, d))
}

/// The god agent's question `command` (made with the session's full id) of
/// the machine whose board lists `session`, through `god::ask`: the
/// machine's name and its answer (gruntas-mcp's gruntas_read). A machine
/// whose presence lapsed is said at once, with nothing sent.
pub fn ask_holder(session: &str, command: impl FnOnce(String) -> HostCommand) -> Result<(String, HostAnswer), Error> {
    use gruntas_harness::sync::god;
    super::host::own_ctx(&super::flags::Flags::default(), |ctx| {
        let k = keys(ctx)?;
        let (_, machines, _) = read(ctx, &k, "the god agent")?;
        let (m, s, device) = holder(&machines, session, "no session")?;
        if m.health != Health::Serving {
            return Err(fail("machine_unknown", format!("{} is UNKNOWN: its presence lapsed, so it was not asked — start gruntas's host service there", m.name)));
        }
        let a = super::synced::announcer(ctx, false)?;
        let answer = god::ask(&a, &device, &m.name, &command(s.clone()), god::ASK_WAIT).map_err(|(code, why)| fail(&code, why))?;
        Ok((m.name.clone(), answer))
    })
}

/// `gruntas ping <machine|session>`.
pub(super) fn ping(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let [target] = args else {
        return Err(fail("bad_args", "`gruntas ping` takes one machine or session, e.g. gruntas ping laptop — gruntas machines and gruntas status list them"));
    };
    let k = keys(ctx)?;
    // A machine by its name or id, else a session on the boards.
    let (api, device, session) = match find_device(&k.chain, target) {
        Ok(Some(d)) => (signed(ctx, &k, "gruntas ping")?, d.id(), None),
        Ok(None) => {
            let (api, machines, _) = read(ctx, &k, "gruntas ping")?;
            let (_, s, d) = holder(&machines, target, "no machine or session")?;
            (api, d, Some(s.clone()))
        }
        Err(e) => return Err(e),
    };
    let name = name_of(&k, &device.to_string());
    // D4: a machine whose presence lapsed is UNKNOWN, said without waiting.
    let presences = match api.list_presences() {
        Ok(p) => p,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    let presence = presences.iter().find(|p| p.device.eq_ignore_ascii_case(&device.to_string()));
    if !presence.is_some_and(|p| p.online) {
        let seen = presence.map_or_else(|| "not announced now".to_string(), |p| format!("last seen {}", super::synced::ago(&p.updated_at, jiff::Timestamp::now())));
        let mut e = fail("machine_unknown", format!("{name} is UNKNOWN: its presence lapsed ({seen}), so it was not pinged — start gruntas's host service there, or gruntas machines always-on on"));
        let mut details = json!({"machine": name, "device": device.to_string(), "health": Health::Unknown});
        if let Some(s) = &session {
            details["session"] = json!(s);
        }
        e.body.insert("details".into(), details);
        return Err(e);
    }
    let id = command_id();
    let sealed = host_command::seal_command(&HostCommand::Ping { session: session.clone() }, &id, &k.device, &device, &k.user, k.chain.generation()).map_err(|e| fail("seal_failed", e))?;
    let started = std::time::Instant::now();
    api.send_host_command(&device.to_string(), &id, &sealed)?;
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stderr, "gruntas: pinged {name} — it answers at its next presence refresh, within about 10 s (waiting up to {} s)…", ANSWER_WAIT.as_secs());
    }
    let deadline = started + ANSWER_WAIT;
    let answer = loop {
        match api.host_command(&device.to_string(), &id) {
            Ok(s) if s.answered => {
                let Some(sealed) = s.answer else { return Err(fail("bad_answer", format!("{name} answered with nothing this gruntas reads"))) };
                break host_command::open_answer(&sealed, &id, &device, &k.device, &k.user, &k.chain).map_err(|e| fail("bad_answer", e))?;
            }
            Ok(_) => {}
            Err(e) if unreachable(&e) => {}
            Err(e) => return Err(e),
        }
        if std::time::Instant::now() >= deadline {
            return Err(fail("host_silent", format!("{name} didn't answer within {} s, though its presence is live", ANSWER_WAIT.as_secs())));
        }
        std::thread::sleep(ANSWER_EVERY);
    };
    let took = started.elapsed().as_secs_f64();
    let pong = match answer {
        HostAnswer::Pong { session: s } => s,
        // An older gruntas there knows no ping.
        HostAnswer::Refused { code, message } => return Err(fail(if code == "unknown_command" { "host_outdated" } else { "host_refused" }, printable(&message))),
        _ => return Err(fail("bad_answer", format!("{name} answered the ping as something else"))),
    };
    let quiet = pong.as_ref().and_then(SessionPing::quiet_for);
    let session_json = session.as_ref().map(|s| {
        let p = pong.as_ref();
        json!({"id": s, "health": p.map_or(Health::Unknown, |p| p.health), "lastActivity": p.and_then(|p| p.last_activity.as_ref()), "quietForSeconds": quiet.map(|q| q.as_secs())})
    });
    if ctx.format != Format::Human {
        let data = json!({"machine": name, "device": device.to_string(), "health": Health::Serving, "answeredInSeconds": (took * 10.0).round() / 10.0, "session": session_json});
        return super::sessions::emit_data(ctx, data, format!("{name} answered in {took:.1} s"));
    }
    let out = &mut *ctx.io.stdout;
    match (&session, &pong) {
        (Some(s), Some(p)) => {
            let last = quiet.map_or_else(|| "no activity known".to_string(), |q| format!("last activity {} ago", span(q)));
            let _ = writeln!(out, "{} on {name}: answered in {took:.1} s  {}  {last}", printable(s), health_word(p.health));
        }
        (Some(s), None) => {
            let _ = writeln!(out, "{} on {name}: answered in {took:.1} s  UNKNOWN  its host says nothing of it", printable(s));
        }
        (None, _) => {
            let _ = writeln!(out, "{name}: answered in {took:.1} s");
        }
    }
    Ok(())
}

/// gruntas-mcp's gruntas_status (GA03): every machine's board, read and
/// opened as `gruntas status` reads them, and the devices whose board did
/// not open.
pub(crate) fn boards() -> Result<(Vec<Machine>, Vec<Unopened>), Error> {
    super::host::own_ctx(&super::flags::Flags::default(), |ctx| {
        let k = keys(ctx)?;
        let (_, machines, unopened) = read(ctx, &k, "the god agent")?;
        Ok((machines, unopened))
    })
}

/// A machine asked by the god agent: its name, its device and its answer.
pub(crate) type Asked = (String, String, Result<HostAnswer, (String, String)>);

/// gruntas-mcp's gruntas_changes (GA03): `command`, made for each machine
/// with a board by its device id, asked of them all at once through
/// `god::ask`. A machine whose presence lapsed, or whose board does not
/// open here, is answered `machine_unknown` at once, with nothing sent.
pub(crate) fn ask_every(command: impl Fn(&str) -> HostCommand + Sync) -> Result<Vec<Asked>, Error> {
    use gruntas_harness::sync::god;
    super::host::own_ctx(&super::flags::Flags::default(), |ctx| {
        let k = keys(ctx)?;
        let (_, machines, unopened) = read(ctx, &k, "the god agent")?;
        let a = super::synced::announcer(ctx, false)?;
        let unknown = |why: String| Err(("machine_unknown".to_string(), why));
        let mut asked: Vec<Asked> = std::thread::scope(|s| {
            let threads: Vec<_> = machines
                .iter()
                .map(|m| {
                    let (a, command) = (&a, &command);
                    s.spawn(move || {
                        let answer = match gruntas_client::e2e::DeviceId::parse(&m.device) {
                            _ if m.health != Health::Serving => unknown(format!("{} is UNKNOWN: its presence lapsed, so it was not asked", m.name)),
                            None => unknown(format!("{}'s board names no device id", m.name)),
                            Some(d) => god::ask(a, &d, &m.name, &command(&m.device), god::ASK_WAIT),
                        };
                        (m.name.clone(), m.device.clone(), answer)
                    })
                })
                .collect();
            threads.into_iter().zip(&machines).map(|(t, m)| t.join().unwrap_or_else(|_| (m.name.clone(), m.device.clone(), Err(("machine_unknown".into(), format!("asking {} failed here", m.name)))))).collect()
        });
        asked.extend(unopened.into_iter().map(|(d, n, e)| (n.clone(), d, unknown(format!("{n}'s status board does not open here: {e}")))));
        Ok(asked)
    })
}

/// gruntas-mcp's gruntas_ping (GA03): `gruntas ping <target> --json`'s data,
/// run in this process.
pub(crate) fn ping_data(target: &str) -> Result<Value, Error> {
    let (mut out, mut err) = (Vec::new(), std::io::sink());
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    {
        let mut io = super::Io { stdout: &mut out, stderr: &mut err, env: &env, tty: false, err_tty: false, stdin_tty: false };
        let mut f = super::flags::Flags::default();
        f.quiet = true;
        let mut ctx = Ctx { io: &mut io, f, format: Format::Json, colour: false, filter: None };
        ping(&mut ctx, &[target.to_string()])?;
    }
    serde_json::from_slice(&out).map_err(|e| fail("bad_answer", format!("gruntas ping did not print JSON: {e}")))
}
