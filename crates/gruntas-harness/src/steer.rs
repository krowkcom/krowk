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
    /// The session that spawned it, when it is a subagent: it takes its
    /// attendance from there when its own record says none.
    pub subagent_of: Option<String>,
    pub attendance: Option<Attendance>,
}

/// Whether `session` is unattended: its own record says so, or it is a
/// subagent (a few levels deep at most) of one that is.
fn unattended_here(sessions: &Path, session: &str) -> bool {
    let mut at = session.to_string();
    for _ in 0..8 {
        let Some(s) = start_of(sessions, &at) else { return false };
        match (s.attendance, s.subagent_of) {
            (Some(a), _) => return a == Attendance::Unattended,
            (None, Some(up)) => at = up,
            (None, None) => return false,
        }
    }
    false
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
        LogBody::SessionStarted { parent_session_id, started_by_session_id, attendance, .. } if e.session_id == session => Some(Start { parent: started_by_session_id.or(parent_session_id.clone()), subagent_of: parent_session_id, attendance }),
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
    let unattended = o.attendance == Some(Attendance::Unattended) || unattended_here(sessions, from);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::LogEvent;

    const TARGET: &str = "01a0ec7b-0000-7000-8000-0000000000a1";
    const PARENT: &str = "01a0ec7b-0000-7000-8000-0000000000b1";
    const ATTENDED: &str = "01a0ec7b-0000-7000-8000-0000000000c1";
    const UNATTENDED: &str = "01a0ec7b-0000-7000-8000-0000000000c2";
    const STRANGER: &str = "01a0ec7b-0000-7000-8000-0000000000c3";

    fn start(home: &Path, id: &str, parent: Option<&str>, subagent: Option<&str>, attendance: Option<Attendance>) {
        let dir = home.join("sessions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let e = LogEvent {
            id: id.into(),
            parent_id: None,
            session_id: id.into(),
            time_ms: 1,
            body: LogBody::SessionStarted { cwd: "/r".into(), gruntas_version: "t".into(), protocol_version: 1, parent_session_id: subagent.map(String::from), agent: None, attendance, started_by_session_id: parent.map(String::from) },
        };
        std::fs::write(dir.join(crate::log::EVENTS_FILE), format!("{}\n", serde_json::to_string(&e).unwrap())).unwrap();
    }

    /// A home with the target, started by `PARENT`, and an attended and an
    /// unattended session; `stranger` has no log here.
    fn home(name: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("gruntas-steer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        start(&home, TARGET, Some(PARENT), None, Some(Attendance::Attended));
        start(&home, PARENT, None, None, Some(Attendance::Unattended));
        start(&home, ATTENDED, None, None, Some(Attendance::Attended));
        start(&home, UNATTENDED, None, None, Some(Attendance::Unattended));
        home
    }

    fn agent(session: Option<&str>) -> Origin {
        Origin { kind: OriginKind::Agent, session_id: session.map(String::from), device: None, attendance: None }
    }

    /// R-SEC-4, D4: each setting against a person, the parent, an attended
    /// and an unattended agent, one that is unknown here, one with no
    /// session, and a command with no origin.
    #[test]
    fn r_sec_4_each_setting_decides_by_origin() {
        let home = home("matrix");
        let sessions = home.join("sessions");
        let person = Origin { kind: OriginKind::Person, session_id: None, device: Some("laptop".into()), attendance: None };
        let says_unattended = Origin { attendance: Some(Attendance::Unattended), ..agent(Some(STRANGER)) };
        // (who, children, attended, any)
        let cases: Vec<(&str, Option<Origin>, [bool; 3])> = vec![
            ("a person", Some(person), [true, true, true]),
            ("no origin: an older client", None, [true, true, true]),
            ("the parent, though it is unattended", Some(agent(Some(PARENT))), [true, true, true]),
            ("an attended agent", Some(agent(Some(ATTENDED))), [false, true, true]),
            ("an unattended agent", Some(agent(Some(UNATTENDED))), [false, false, true]),
            ("an agent on another machine, attended by its word", Some(agent(Some(STRANGER))), [false, true, true]),
            ("an agent on another machine, unattended by its word", Some(says_unattended), [false, false, true]),
            ("an agent with no session", Some(agent(None)), [false, false, true]),
        ];
        for (who, origin, expect) in cases {
            for (setting, ok) in [Setting::Children, Setting::Attended, Setting::Any].into_iter().zip(expect) {
                let got = allow(setting, origin.as_ref(), TARGET, &sessions);
                assert_eq!(got.is_ok(), ok, "{who} under {}: {got:?}", setting.name());
                if let Err(why) = got {
                    assert!(why.contains(&format!("steer setting is {}", setting.name())) && why.contains("gruntas config set steer") && why.contains("on a terminal"), "{why}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A session with no parent (a start from a terminal) is steerable by
    /// no agent under `children`; one started by a subagent's spawn counts
    /// that session as its parent too.
    #[test]
    fn r_sec_4_a_session_with_no_parent_is_steered_by_nobody_under_children() {
        let home = home("parentless");
        let sessions = home.join("sessions");
        start(&home, STRANGER, None, None, None);
        assert!(allow(Setting::Children, Some(&agent(Some(PARENT))), STRANGER, &sessions).is_err());
        assert!(allow(Setting::Attended, Some(&agent(Some(ATTENDED))), STRANGER, &sessions).is_ok());
        start(&home, STRANGER, None, Some(PARENT), None);
        // A subagent of an unattended session is unattended with it.
        assert!(allow(Setting::Attended, Some(&agent(Some(STRANGER))), TARGET, &sessions).is_err(), "the unattended label carries down to a subagent");
        assert!(allow(Setting::Children, Some(&agent(Some(PARENT))), STRANGER, &sessions).is_ok(), "a subagent's parent steers it");
        // No log at all: no parent either.
        assert!(allow(Setting::Children, Some(&agent(Some(PARENT))), "01a0ec7b-0000-7000-8000-0000000000ff", &sessions).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The setting is read from the config beside the sessions directory,
    /// as each command arrives; none is `attended`, and a file or value that
    /// cannot be read is `children`, never looser.
    #[test]
    fn r_sec_5_the_setting_is_read_from_the_global_config_and_fails_closed() {
        let home = home("read");
        let sessions = home.join("sessions");
        let config = home.join(gruntas_api::home::CONFIG);
        assert_eq!(read(&sessions), Setting::Attended, "none named");
        for (text, want) in [(r#"{"steer":"any"}"#, Setting::Any), (r#"{"steer":"children"}"#, Setting::Children), (r#"{"other":1}"#, Setting::Attended), (r#"{"steer":null}"#, Setting::Attended), (r#"{"steer":"everyone"}"#, Setting::Children), (r#"{"steer":7}"#, Setting::Children), ("not json", Setting::Children)] {
            std::fs::write(&config, text).unwrap();
            assert_eq!(read(&sessions), want, "{text}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// What is steering: a prompt to an existing session, steer, interrupt,
    /// approve, a model switch; not a prompt that starts a session, a
    /// switch with no session, nor an image.
    #[test]
    fn r_sec_4_only_commands_on_another_session_are_steering() {
        let o = Some(Box::new(agent(Some(PARENT))));
        let prompt = |id: Option<&str>| Command::Prompt { session_id: id.map(String::from), text: "t".into(), images: Vec::new(), model: None, permission_mode: Default::default(), toolset: None, effort: None, rollover: None, budget: None, attendance: None, origin: o.clone() };
        assert_eq!(steers(&prompt(Some(TARGET))).map(|(s, _)| s), Some(TARGET));
        assert!(steers(&prompt(None)).is_none(), "a prompt that starts a session steers nothing");
        let model = crate::protocol::ModelRef { instance: "i".into(), model: "m".into() };
        assert!(steers(&Command::SwitchModel { session_id: None, model: model.clone(), origin: o.clone() }).is_none());
        for c in [
            Command::SwitchModel { session_id: Some(TARGET.into()), model, origin: o.clone() },
            Command::Steer { session_id: TARGET.into(), text: "t".into(), images: Vec::new(), origin: o.clone() },
            Command::Interrupt { session_id: TARGET.into(), origin: o.clone() },
            Command::Approve { session_id: TARGET.into(), request_id: "r".into(), decision: crate::protocol::ApprovalDecision::Allow, answers: Vec::new(), origin: o.clone() },
        ] {
            assert_eq!(steers(&c).map(|(s, origin)| (s, origin.map(|o| o.kind))), Some((TARGET, Some(OriginKind::Agent))), "{c:?}");
        }
        assert!(steers(&Command::PutImage { session_id: Some(TARGET.into()), sha256: String::new(), media_type: "image/png".into(), data: String::new() }).is_none());
    }

    /// D6: attach is a person only when its input is a terminal, and the
    /// origin it stamps replaces whatever a typed command carried.
    #[test]
    fn r_sec_5_attach_is_a_person_only_on_a_terminal() {
        let (tty, piped) = (attach_origin(true, Some("laptop".into())), attach_origin(false, Some("laptop".into())));
        assert_eq!((tty.kind, tty.session_id.as_deref(), tty.device.as_deref()), (OriginKind::Person, None, Some("laptop")));
        assert_eq!((piped.kind, piped.session_id.as_deref()), (OriginKind::Agent, None));
        let mut c = Command::Interrupt { session_id: TARGET.into(), origin: Some(Box::new(agent(Some(PARENT)))) };
        stamp(&mut c, &piped);
        assert!(matches!(c, Command::Interrupt { origin: Some(o), .. } if *o == piped));
    }
}
