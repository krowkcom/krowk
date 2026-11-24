//! gruntas_message and gruntas_interrupt (GA05, R-GOD-6): a `message` or
//! `interrupt` host command to the machine that holds the session, carrying
//! this session's origin (an agent's, with its session id, attendance and
//! this machine's name); the target daemon checks it against its `steer`
//! setting and delivers it.

use super::super::{arguments, this_session, Outcome};
use gruntas_api::fail;
use gruntas_client::host_command::{refusal, slug_shaped, HostAnswer, HostCommand, MAX_STEER_TEXT};
use gruntas_client::protocol::{Origin, OriginKind, Registration};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    session: String,
    text: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InterruptArgs {
    session: String,
}

pub(super) fn message(args: &Value) -> Outcome {
    let a: MessageArgs = arguments(args, "gruntas_message takes a session and a text")?;
    if a.text.trim().is_empty() || a.text.len() > MAX_STEER_TEXT {
        return Err(fail("bad_arguments", format!("text is what to tell the session: not empty, at most {MAX_STEER_TEXT} bytes")));
    }
    let o = origin(here().as_ref(), crate::cli::this_machine());
    let asked = crate::cli::ask_holder(a.session.trim(), |session| HostCommand::Message { session, text: a.text, origin: o })?;
    said(a.session.trim(), asked)
}

pub(super) fn interrupt(args: &Value) -> Outcome {
    let a: InterruptArgs = arguments(args, "gruntas_interrupt takes a session")?;
    let o = origin(here().as_ref(), crate::cli::this_machine());
    let asked = crate::cli::ask_holder(a.session.trim(), |session| HostCommand::Interrupt { session, origin: o })?;
    said(a.session.trim(), asked)
}

/// The session this server runs in, as its plugin registered it.
fn here() -> Option<Registration> {
    this_session(&|k| std::env::var(k).unwrap_or_default())
}

/// Where a steer from the session `from`, on the machine `me`, comes from:
/// an agent, always, so the target holds it to its `steer` setting.
fn origin(from: Option<&Registration>, me: Option<String>) -> Origin {
    Origin { kind: OriginKind::Agent, session_id: from.map(|r| r.session_id.clone()), device: me, attendance: from.and_then(|r| r.attendance) }
}

/// The target's answer as the tool says it: delivered, nothing running, or
/// its refusal as a tool error under its own code.
fn said(session: &str, (machine, answer): (String, HostAnswer)) -> Outcome {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let (session, machine) = (clean(session), clean(&machine));
    match &answer {
        HostAnswer::Delivered { .. } => Ok((format!("Delivered to session {session} on {machine}."), json!({ "delivered": true, "session": session, "machine": machine }))),
        HostAnswer::Refused { code, .. } if code == refusal::NOTHING_RUNNING => Ok((format!("Nothing is running in session {session} on {machine}: there was no turn to stop."), json!({ "delivered": false, "nothingRunning": true, "session": session, "machine": machine }))),
        HostAnswer::Refused { code, .. } => Err(fail(if slug_shaped(code) { code } else { "refused" }, clean(&answer.refusal_line(&machine).unwrap_or_default()))),
        _ => Err(fail("bad_answer", format!("{machine} answered the steer as another command"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::protocol::{Attendance, PluginHarness};

    const SESSION: &str = "01a0ec7b-0000-7000-8000-0000000000a1";

    /// GA05 (R-GOD-6): a steer carries this session's origin, an agent's,
    /// and the tool says what the target answered: delivered, nothing
    /// running, or its refusal under its own code.
    #[test]
    fn ga05_a_steer_carries_this_sessions_origin() {
        let r = Registration { session_id: "01a0ec7b-0000-7000-8000-0000000000c1".into(), harness: PluginHarness::Claude, harness_pid: Some(7), config_home: None, cwd: None, repository: None, transcript_path: None, attendance: Some(Attendance::Unattended) };
        let o = origin(Some(&r), Some("laptop".into()));
        assert_eq!(o, Origin { kind: OriginKind::Agent, session_id: Some(r.session_id.clone()), device: Some("laptop".into()), attendance: Some(Attendance::Unattended) });
        assert_eq!(origin(None, None).kind, OriginKind::Agent);

        let at = |a: HostAnswer| said(SESSION, ("desk".into(), a));
        let (text, v) = at(HostAnswer::Delivered { session: SESSION.into() }).unwrap();
        assert_eq!((text.as_str(), &v["delivered"]), ("Delivered to session 01a0ec7b-0000-7000-8000-0000000000a1 on desk.", &json!(true)));
        let (text, v) = at(HostAnswer::refused(refusal::NOTHING_RUNNING, "nothing is running")).unwrap();
        assert!(text.starts_with("Nothing is running") && v["nothingRunning"] == true, "{text}");
        let e = at(HostAnswer::refused(refusal::STEER_REFUSED, "session x may not steer it\nfor sure")).unwrap_err();
        assert_eq!((e.code(), e.fix()), (refusal::STEER_REFUSED.to_string(), "desk: session x may not steer itfor sure".to_string()));
        assert_eq!(at(HostAnswer::refused("not a code!", "no")).unwrap_err().code(), "refused");
        assert_eq!(at(HostAnswer::Pong { session: None }).unwrap_err().code(), "bad_answer");
    }
}
