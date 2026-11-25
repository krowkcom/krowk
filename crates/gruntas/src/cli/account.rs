//! `/gruntas:account`, the gruntas plugin's mod's command (Rollover for
//! launched sessions D8, R-RL-8): the mod runs `gruntas hook claude account
//! <session> [<name> | --cancel]` from inside the session, so this process's
//! parent is the harness (`hook::harness_pid`).
//!
//! With no name it lists the session's harness's accounts as `gruntas
//! providers list --check` does, with what each is signed in as and has
//! left, and marks the one the session runs on. With a name, in a session
//! `gruntas run` started (`attended::launched`), it queues that account as
//! the session's next (`attended::queue`, the record the ladder's pick
//! takes, which it then never replaces): after `/exit` the Launcher resumes
//! the same session there, on the same model. `--cancel` drops what is
//! queued. A move goes only to a ready account of the same harness signed
//! in as another login; anything else is refused with a sentence naming why
//! and the accounts it could go to.
//!
//! What it prints is the command's output; it exits 1 when it refused.

use super::status::{self, Standing};
use super::Ctx;
use crate::output::Format;
use gruntas_harness::attended::{self, Next};
use gruntas_harness::instances::Registry;
use gruntas_harness::ladder::Rung;
use gruntas_harness::launch::Harness;
use gruntas_harness::readiness::Report;

/// `gruntas hook claude account <session> [<name> | --cancel]`: the
/// output on stdout, and the exit code.
pub fn hook(args: &[String]) -> i32 {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let pid = gruntas_harness::hook::harness_pid();
    // In a session the Launcher started on a named account, the harness's
    // CLAUDE_CONFIG_DIR is that account's: read as the environment's, it
    // would make the default account (`claude`) look like it. On the
    // default account it is the default's own, inherited or not.
    let named = gruntas_harness::daemon::dir(&var).ok().zip(pid).and_then(|(d, p)| attended::launched(&d, p)).is_some_and(|l| l.account != Harness::Claude.name());
    let env = |k: &str| if named && k == "CLAUDE_CONFIG_DIR" { String::new() } else { var(k) };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut io = super::Io { stdout: &mut out, stderr: &mut err, env: &env, tty: false, err_tty: false, stdin_tty: false };
    let mut ctx = Ctx { io: &mut io, f: Default::default(), format: Format::Human, colour: false, filter: None };
    let (said, code) = match run(&mut ctx, args, pid, &var) {
        Ok(s) => (s, 0),
        Err(s) => (s, 1),
    };
    let _ = writeln!(ctx.io.stdout, "{said}");
    code
}

/// What the command says, or why it refused; `pid` is the harness's.
/// `var` is the harness's own environment.
fn run(ctx: &mut Ctx, args: &[String], pid: Option<u32>, var: &dyn Fn(&str) -> String) -> Result<String, String> {
    let (session, rest) = match args {
        [h, _, session, rest @ ..] if h == "claude" => (session.as_str(), rest),
        _ => return Err("usage: gruntas hook claude account <session> [<account> | --cancel]".into()),
    };
    let harness = Harness::Claude;
    // It names the session `/exit` resumes (`--resume <session>`).
    if !gruntas_harness::handoff::valid_session_id(session) || session.starts_with('-') {
        return Err(format!("{session:?} is not a session id"));
    }
    let (_, registry, reports) = status::reports(ctx).map_err(|e| e.to_string())?;
    let data = status::usage(ctx, &registry).map_err(|e| e.to_string())?;
    let now = gruntas_store::now_ms();
    let (reports, others): (Vec<Report>, Vec<Report>) = reports.into_iter().partition(|r| r.kind == harness.kind());
    let daemon = gruntas_harness::daemon::dir(ctx.io.env).ok();
    // The account the session runs on: the one the Launcher started it on,
    // else the one whose home the harness runs in.
    let launched = daemon.as_deref().zip(pid).and_then(|(d, p)| attended::launched(d, p));
    let current = match &launched {
        Some(l) => Some(registry.current(&l.account)),
        None => here(var, &registry, &reports),
    };
    let standings: Vec<Standing> = reports.iter().map(|r| status::standing(r, &reports, &data, now)).collect();
    let login = |name: &str| reports.iter().position(|r| r.instance == name).and_then(|i| standings[i].login.clone()).unwrap_or_else(|| name.to_string());
    // Where a move can go: ready, not the account it is on, nor its login.
    let choices: Vec<&str> = reports
        .iter()
        .filter(|r| r.readiness.is_ready() && current.as_deref().is_none_or(|c| r.instance != c && login(&r.instance) != login(c)))
        .map(|r| r.instance.as_str())
        .collect();
    let valid = || match choices.is_empty() {
        true => format!("no other {} account is ready on another login — sign one in with `gruntas connect`", harness.name()),
        false => format!("choose one of {}", choices.join(", ")),
    };
    let queued = daemon.as_deref().zip(pid).and_then(|(d, p)| attended::queued(d, p));
    match rest {
        [] => Ok(list(&reports, &standings, current.as_deref(), queued.as_ref())),
        // The mod's picker: every connected account, those a move can go to
        // first (the most left in its tightest window first, unknown after),
        // the rest after them with why not.
        [flag] if flag == "--choices" => {
            let mut rows: Vec<(serde_json::Value, Option<f64>)> = reports
                .iter()
                .zip(&standings)
                .map(|(r, s)| {
                    let name = r.instance.as_str();
                    // Why not, as a sentence for the toast and in a few words for the row.
                    let (why, note) = if Some(name) == current.as_deref() {
                        (Some("this session runs on it".to_string()), "this session".to_string())
                    } else if !r.readiness.is_ready() {
                        (Some(format!("it is {}", r.readiness.label())), r.readiness.label().to_string())
                    } else if let Some(c) = current.as_deref().filter(|c| login(name) == login(c)) {
                        (Some(format!("it is signed in as the same login as {c}, so moving there gains nothing")), format!("same login as {c}"))
                    } else {
                        (None, s.brief())
                    };
                    let left = if why.is_none() { s.least_left().or(Some(-1.0)) } else { None };
                    (serde_json::json!({ "account": name, "label": format!("{name} · {note}"), "why": why }), left)
                })
                .collect();
            rows.extend(others.iter().map(|r| {
                let why = "it is a Codex account, and a Claude Code session moves only to another Claude Code account";
                (serde_json::json!({ "account": r.instance, "label": format!("{} · Codex account", r.instance), "why": why }), None)
            }));
            // Stable: equal ones keep the list's order.
            rows.sort_by(|a, b| b.1.unwrap_or(f64::NEG_INFINITY).total_cmp(&a.1.unwrap_or(f64::NEG_INFINITY)));
            let choices: Vec<serde_json::Value> = rows.into_iter().map(|(v, _)| v).collect();
            // Where there is no picker, the list it shows instead, so the mod asks once.
            let text = launched.is_none().then(|| list(&reports, &standings, current.as_deref(), queued.as_ref()));
            Ok(serde_json::json!({ "launched": launched.is_some(), "current": current, "queued": queued.map(|n| n.account), "choices": choices, "list": text }).to_string())
        }
        [flag] if flag == "--cancel" => {
            let (Some(d), Some(p), Some(n)) = (daemon.as_deref(), pid, queued) else { return Ok("Nothing is queued: /exit ends this session as usual.".into()) };
            std::fs::remove_file(attended::next_path(d, p)).map_err(|e| format!("{} stays queued: {e}", n.account))?;
            Ok(format!("{} is no longer queued: /exit ends this session as usual.", n.account))
        }
        [name] => {
            let (Some(d), Some(p), Some(l)) = (daemon.as_deref(), pid, launched) else {
                return Err(format!("This session was not started by gruntas, so it cannot move to another account: start Claude Code with `gruntas run {}` to be able to.", harness.name()));
            };
            // Only this harness's own session, where the daemon knows which
            // runs where: one registered elsewhere, or not among this
            // harness's (it registers each new one, after a /clear too).
            let sessions = gruntas_harness::daemon::registry::Registry::read(d);
            let mut here = sessions.sessions().filter(|r| r.harness_pid == Some(p)).peekable();
            let elsewhere = sessions.get(session).is_some_and(|r| r.harness_pid != Some(p));
            if elsewhere || (here.peek().is_some() && !here.any(|r| r.session_id == session)) {
                return Err(format!("Session {session} is not the one this Claude Code runs."));
            }
            let to = registry.current(name);
            let Some(inst) = registry.instances.get(&to).filter(|i| i.backend.is_some()) else { return Err(format!("There is no account named {name}: {}.", valid())) };
            if inst.kind != harness.kind() {
                return Err(format!("{name} is not a {} account, and this session moves only to another of its own harness's: {}.", harness.name(), valid()));
            }
            let from = registry.current(&l.account);
            if to == from {
                return Err(format!("This session already runs on {name}: {}.", valid()));
            }
            let r = reports.iter().find(|r| r.instance == to).ok_or_else(|| format!("{name} could not be checked: {}.", valid()))?;
            if !r.readiness.is_ready() {
                let why = r.fix.as_ref().map(|f| format!(" — {f}")).unwrap_or_default();
                return Err(format!("{name} is not ready ({}{why}): {}.", r.readiness.label(), valid()));
            }
            if login(&to) == login(&from) {
                return Err(format!("{name} is signed in as the same login as {from}, so moving there gains nothing: {}.", valid()));
            }
            let transcript = transcript(d, session, &registry, &from);
            let next = Next { session: session.to_string(), account: to.clone(), model: l.model, rung: Rung::SameHarness, transcript, picked: true };
            attended::queue(d, p, &next).map_err(|e| format!("{to} could not be queued: {e}"))?;
            Ok(format!("Type /exit to continue on {to}."))
        }
        _ => Err("usage: /gruntas:account [<account> | --cancel]".into()),
    }
}

/// The accounts, one per line, `*` on the one the session runs on, and
/// what is queued.
fn list(reports: &[Report], standings: &[Standing], current: Option<&str>, queued: Option<&Next>) -> String {
    let w = reports.iter().map(|r| r.instance.len()).max().unwrap_or(0);
    let wl = reports.iter().map(|r| r.readiness.label().len()).max().unwrap_or(0);
    let mut out: Vec<String> = reports
        .iter()
        .zip(standings)
        .map(|(r, s)| {
            let mark = if Some(r.instance.as_str()) == current { "*" } else { " " };
            format!("{mark} {:<w$}  {:<wl$}  {}", r.instance, r.readiness.label(), s.line())
        })
        .collect();
    if current.is_none() {
        out.push("This session's account is not one gruntas knows.".into());
    }
    if let Some(n) = queued {
        out.push(format!("Queued: /exit continues on {} (`/gruntas:account --cancel` drops it).", n.account));
    }
    out.join("\n")
}

/// The account whose home this harness runs in (`CLAUDE_CONFIG_DIR`, else
/// `~/.claude`), for a session gruntas did not start.
fn here(var: &dyn Fn(&str) -> String, registry: &Registry, reports: &[Report]) -> Option<String> {
    let home = gruntas_harness::hook::config_home(gruntas_harness::protocol::PluginHarness::Claude, var)?;
    let key = gruntas_harness::usage::home_key(&home);
    let at = |n: &&String| registry.instances.get(*n).and_then(|i| i.backend.as_ref()).and_then(|b| b.home.as_ref()).is_some_and(|h| gruntas_harness::usage::home_key(&h.display().to_string()) == key);
    // A named account whose home it is, before the default, whose home is
    // the environment's.
    let mut names: Vec<&String> = reports.iter().map(|r| &r.instance).collect();
    names.sort_by_key(|n| n.as_str() == "claude");
    names.into_iter().find(at).cloned()
}

/// The session's transcript: where the daemon's registration says, else
/// where Claude Code keeps it under `from`'s home, for this directory.
fn transcript(daemon: &std::path::Path, session: &str, registry: &Registry, from: &str) -> Option<String> {
    use gruntas_harness::daemon::registry::Registry as Sessions;
    Sessions::read(daemon).get(session).and_then(|r| r.transcript_path.clone()).or_else(|| {
        let home = registry.instances.get(from)?.backend.as_ref()?.home.clone()?;
        let cwd = std::env::current_dir().ok()?;
        Some(gruntas_harness::claude::transcript_path(&home, &cwd.display().to_string(), session).display().to_string())
    })
}
