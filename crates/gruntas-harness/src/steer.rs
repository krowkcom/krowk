//! The `steer` setting (D4, D6, R-SEC-4): how far an agent's steering
//! reaches, and the host's check of it.
//!
//! "Steering" is any command that acts on a session other than the
//! caller's own: a prompt to an existing session, `steer`, `interrupt`,
//! `approve`, `switchModel` and `take`. The host checks each one here, after
//! the checks that already stood (a viewer's session key, a host command's
//! signature), never instead of them.
//!
//! - `children`: only the target's parent steers it.
//! - `attended` (the default): the target's parent, or a steering session
//!   that is attended.
//! - `any`: anyone.
//!
//! A person origin always passes, and so does a command with no origin: an
//! older client, or an app that sends none, cannot be told from a person's
//! device, and refusing it would break every viewer that predates origins.
//! An agent origin that names no session (a `gruntas sync attach` whose
//! input is no terminal) passes under `any` alone.
//!
//! A session's parent is the session that started it (`startedBySessionId`,
//! a host command's sending session or an agent's prompt) or, for a
//! subagent, the session that spawned it. A session that records neither
//! has no parent, and is steerable under `attended` and `any` only.
//!
//! Attendance is a label gruntas assigns, read from the steering session's
//! start record on this machine, or from the word of the origin when the
//! session runs on another one; a session the host has no word of counts as
//! attended.

use crate::protocol::{Attendance, Command, LogBody, Origin, OriginKind};
use std::path::Path;

/// The setting's key in the global config.
pub const KEY: &str = "steer";

/// How far an agent's steering reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    Children,
    #[default]
    Attended,
    Any,
}

impl Setting {
    pub const NAMES: [&'static str; 3] = ["children", "attended", "any"];

    pub fn name(self) -> &'static str {
        match self {
            Setting::Children => "children",
            Setting::Attended => "attended",
            Setting::Any => "any",
        }
    }

    pub fn parse(s: &str) -> Result<Setting, String> {
        match s.trim() {
            "children" => Ok(Setting::Children),
            "attended" => Ok(Setting::Attended),
            "any" => Ok(Setting::Any),
            other => Err(format!("{other:?} is not a steer setting — use {}", Setting::NAMES.join(", "))),
        }
    }
}

/// The setting as the global config names it: the config sits beside the
/// `sessions` directory, in gruntas's home. None named is the default; a
/// file or value that cannot be read is the strictest, `children`, never
/// the loosest, so a broken config does not widen anyone's reach.
pub fn read(sessions: &Path) -> Setting {
    let Some(file) = sessions.parent().map(|home| home.join(gruntas_api::home::CONFIG)) else { return Setting::default() };
    let raw = match std::fs::read(&file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Setting::default(),
        Err(_) => return Setting::Children,
    };
    match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(v) => match v.get(KEY).filter(|v| !v.is_null()) {
            None => Setting::default(),
            Some(v) => v.as_str().and_then(|s| Setting::parse(s).ok()).unwrap_or(Setting::Children),
        },
        Err(_) => Setting::Children,
    }
}

/// What a session's start record says that the check reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Start {
    pub parent: Option<String>,
    pub attendance: Option<Attendance>,
}

/// `session`'s start record on this machine, the first line of its log; none
/// when there is no log, or it begins with anything else.
pub fn start_of(sessions: &Path, session: &str) -> Option<Start> {
    use std::io::BufRead;
    if !crate::log::valid_id(session) {
        return None;
    }
    let f = std::fs::File::open(sessions.join(session).join(crate::log::EVENTS_FILE)).ok()?;
    let first = std::io::BufReader::new(f).lines().next()?.ok()?;
    let e: crate::protocol::LogEvent = serde_json::from_str(&first).ok()?;
    match e.body {
        LogBody::SessionStarted { parent_session_id, started_by_session_id, attendance, .. } if e.session_id == session => Some(Start { parent: started_by_session_id.or(parent_session_id), attendance }),
        _ => None,
    }
}

/// The session a steering command acts on, and the origin it carries; none
/// for a command that is not steering.
pub fn steers(c: &Command) -> Option<(&str, Option<&Origin>)> {
    match c {
        Command::Prompt { session_id: Some(s), origin, .. } | Command::SwitchModel { session_id: Some(s), origin, .. } => Some((s, origin.as_deref())),
        Command::Steer { session_id, origin, .. } | Command::Interrupt { session_id, origin } | Command::Approve { session_id, origin, .. } => Some((session_id, origin.as_deref())),
        _ => None,
    }
}

/// Whether `origin` may steer `target` under `setting`, `Err` with the
/// refusal, which names the setting and how a person changes it.
pub fn allow(setting: Setting, origin: Option<&Origin>, target: &str, sessions: &Path) -> Result<(), String> {
    let refuse = |who: &str| {
        Err(format!(
            "{who} may not steer session {target}: this machine's steer setting is {}. A person changes it with `gruntas config set steer <{}>` on a terminal",
            setting.name(),
            Setting::NAMES.join("|")
        ))
    };
    let Some(o) = origin.filter(|o| o.kind == OriginKind::Agent) else { return Ok(()) };
    if setting == Setting::Any {
        return Ok(());
    }
    let Some(from) = o.session_id.as_deref() else { return refuse("an agent with no session of its own") };
    if start_of(sessions, target).and_then(|s| s.parent).as_deref() == Some(from) {
        return Ok(());
    }
    let unattended = o.attendance == Some(Attendance::Unattended) || start_of(sessions, from).is_some_and(|s| s.attendance == Some(Attendance::Unattended));
    match setting {
        Setting::Attended if !unattended => Ok(()),
        Setting::Attended => refuse(&format!("session {from}, which runs unattended and did not start it,")),
        _ => refuse(&format!("session {from}, which did not start it,")),
    }
}

/// `allow` for the command `c`, under the setting as the config names it
/// now: a command that is not steering passes.
pub fn check(c: &Command, sessions: &Path) -> Result<(), String> {
    match steers(c) {
        Some((target, origin)) => allow(read(sessions), origin, target, sessions),
        None => Ok(()),
    }
}

/// Where the commands `gruntas sync attach` sends come from (D6): a person
/// only when its input is a terminal, else an agent with no session of its
/// own — a script or an agent's shell piping lines in — which the host holds
/// to the `steer` setting like any other agent.
pub fn attach_origin(terminal: bool, device: Option<String>) -> Origin {
    Origin { kind: if terminal { OriginKind::Person } else { OriginKind::Agent }, session_id: None, device, attendance: None }
}

/// `c` with `origin` as the one it carries, when it is a steering command.
pub fn stamp(c: &mut Command, origin: &Origin) {
    match c {
        Command::Prompt { origin: o, .. } | Command::SwitchModel { origin: o, .. } | Command::Steer { origin: o, .. } | Command::Interrupt { origin: o, .. } | Command::Approve { origin: o, .. } => *o = Some(Box::new(origin.clone())),
        _ => {}
    }
}
