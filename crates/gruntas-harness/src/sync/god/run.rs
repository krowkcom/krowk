//! Steering a gruntas-run session, and telling the kinds apart (GA06, D5):
//! a message is a prompt (with its origin) when the session is idle and a
//! steer when a turn runs; an interrupt is today's interrupt command. A
//! Codex, headless or plugin-less session is refused `cant_steer`, "can't
//! steer this session yet", with the reason.
//!
//! The daemon is asked as any client asks it, over its socket: a steer
//! first, and a prompt when it says no turn is running. The prompt runs in
//! the `default` permission mode, so the turn asks before anything it would
//! not do unasked, and its text is framed with the origin as a steer's is.
//! It is `delivered` once its turn has started; the turn runs on after this
//! client goes.

use super::super::auto::Config;
use super::inbox::Steer;
use super::Target;
use crate::daemon::client::{Client, ConnectError};
use crate::engine::EngineError;
use crate::protocol::{Attendance, Command, LogBody, Origin, PermissionMode, PluginHarness, StreamLine};
use gruntas_client::host_command::{refusal, HostAnswer};
use std::path::Path;

/// Why a Codex session is not steered (the launcher build makes it so).
pub const CODEX: &str = "it is a Codex session, and gruntas delivers nothing to Codex until it starts Codex itself";
/// Why a Claude Code session with no terminal is not.
pub const HEADLESS: &str = "it runs headless, with no terminal, so nothing in it reads a message";
/// Why a Claude Code session whose plugin mod does not answer is not (the
/// inbox's wait, GA05).
pub const NO_MOD: &str = "no gruntas plugin mod reads its inbox; install or update the gruntas plugin in that Claude Code";

/// The refusal for a session that can't be steered yet, saying `why`.
pub fn cant_steer(why: &str) -> HostAnswer {
    HostAnswer::refused(refusal::CANT_STEER, format!("can't steer this session yet: {why}"))
}

/// Which path steers `session`, or the refusal: `no_session` when this
/// machine holds none by that id, `cant_steer` with the reason for a kind
/// that can't be steered yet.
pub(crate) fn target(c: &Config, session: &str) -> Result<Target, HostAnswer> {
    target_in(c.socket.parent().unwrap_or(Path::new("/")), &c.sessions, session)
}

/// `target` over the daemon's directory (its registrations) and the
/// session logs' directory.
pub fn target_in(daemon: &Path, sessions: &Path, session: &str) -> Result<Target, HostAnswer> {
    match crate::daemon::registry::Registry::read(daemon).get(session) {
        Some(r) if r.harness == PluginHarness::Codex => Err(cant_steer(CODEX)),
        Some(r) if r.attendance == Some(Attendance::Unattended) => Err(cant_steer(HEADLESS)),
        Some(_) => Ok(Target::ClaudeCode),
        None if crate::log::valid_id(session) && sessions.join(session).join(crate::log::EVENTS_FILE).is_file() => Ok(Target::GruntasRun),
        None => Err(HostAnswer::refused(refusal::NO_SESSION, format!("this machine has no session {session}"))),
    }
}

/// `s` given to the gruntas-run session `session`: `HostAnswer::Delivered`,
/// or refused (`nothing_running` for an interrupt with no turn).
pub(crate) fn deliver(c: &Config, session: &str, s: &Steer) -> HostAnswer {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return HostAnswer::refused("host_failed", "this machine has no async runtime for the steer — try again");
    };
    rt.block_on(deliver_on(&c.socket, &c.version, session, s))
}

/// `deliver` through the daemon on `socket`.
pub async fn deliver_on(socket: &Path, version: &str, session: &str, s: &Steer) -> HostAnswer {
    // Background, so it never keeps the daemon up; answering no approval,
    // which stay with the session's own clients.
    let d = match Client::connect_as(socket, Path::new("/"), version, false, true).await {
        Ok(d) => d,
        Err(ConnectError::Failed(e)) => return refused(e),
        Err(ConnectError::Absent) => return HostAnswer::refused("host_unavailable", "the host daemon is not running — it starts again with the next gruntas there"),
    };
    let delivered = || HostAnswer::Delivered { session: session.to_string() };
    let origin = Some(Box::new(s.origin().clone()));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    match s {
        Steer::Interrupt { .. } => match d.execute(Command::Interrupt { session_id: session.to_string(), origin }, tx).await {
            Ok(_) => delivered(),
            Err(e) if e.code == "no_running_turn" => HostAnswer::refused(refusal::NOTHING_RUNNING, format!("nothing is running in session {session}")),
            Err(e) => refused(e),
        },
        Steer::Message { text, origin: o, .. } => match d.execute(Command::Steer { session_id: session.to_string(), text: text.clone(), images: Vec::new(), origin }, tx).await {
            Ok(_) => delivered(),
            Err(e) if e.code == "no_running_turn" => prompt(&d, session, text, o).await,
            Err(e) => refused(e),
        },
    }
}

/// `text` sent to the idle `session` as its next prompt, framed with
/// `origin`: delivered once its turn has started.
async fn prompt(d: &Client, session: &str, text: &str, origin: &Origin) -> HostAnswer {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let framed = crate::neutral::steer_framed(text, origin);
    let cmd = Command::Prompt { session_id: Some(session.to_string()), text: framed, images: Vec::new(), model: None, permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None, attendance: None, fenced: false, origin: Some(Box::new(origin.clone())) };
    let run = d.execute(cmd, tx);
    tokio::pin!(run);
    let started = |l: &StreamLine| matches!(l, StreamLine::Log(ev) if matches!(ev.body, LogBody::TurnStarted { .. }));
    let mut closed = false;
    loop {
        tokio::select! {
            // The turn's lines first: they all arrive before its end.
            biased;
            line = rx.recv(), if !closed => match line {
                Some(l) if started(&l) => return HostAnswer::Delivered { session: session.to_string() },
                Some(_) => {}
                None => closed = true,
            },
            done = &mut run => {
                let ran = std::iter::from_fn(|| rx.try_recv().ok()).any(|l| started(&l));
                return match done {
                    Err(e) if !ran => refused(e),
                    _ => HostAnswer::Delivered { session: session.to_string() },
                };
            }
        }
    }
}

fn refused(e: EngineError) -> HostAnswer {
    HostAnswer::refused(&e.code, e.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Registration;

    const SESSION: &str = "01a0ec7b-0000-7000-8000-0000000000a1";

    fn registered(daemon: &Path, harness: PluginHarness, attendance: Option<Attendance>) {
        let mut r = crate::daemon::registry::Registry::default();
        let reg = Registration { session_id: SESSION.into(), harness, harness_pid: None, config_home: None, cwd: Some("/r".into()), repository: None, transcript_path: None, attendance };
        r.apply(&crate::protocol::Activity::Register(reg));
        r.save(daemon).unwrap();
    }

    fn refusal_of(t: Result<Target, HostAnswer>) -> (String, String) {
        match t {
            Err(HostAnswer::Refused { code, message }) => (code, message),
            other => panic!("not refused: {other:?}"),
        }
    }

    /// Codex, headless and plugin-less sessions are refused `cant_steer`
    /// with the reason; a Claude Code session with a terminal goes to its
    /// inbox, a gruntas-run one to the daemon.
    #[test]
    fn ga06_other_sessions_are_refused_with_the_reason() {
        let root = std::env::temp_dir().join(format!("gruntas-god-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (daemon, sessions) = (root.join("run"), root.join("sessions"));
        std::fs::create_dir_all(&daemon).unwrap();
        assert_eq!(refusal_of(target_in(&daemon, &sessions, SESSION)).0, refusal::NO_SESSION);
        std::fs::create_dir_all(sessions.join(SESSION)).unwrap();
        std::fs::write(sessions.join(SESSION).join(crate::log::EVENTS_FILE), "").unwrap();
        assert_eq!(target_in(&daemon, &sessions, SESSION), Ok(Target::GruntasRun));
        registered(&daemon, PluginHarness::Claude, None);
        assert_eq!(target_in(&daemon, &sessions, SESSION), Ok(Target::ClaudeCode));
        for (harness, attendance, why) in [(PluginHarness::Codex, None, CODEX), (PluginHarness::Claude, Some(Attendance::Unattended), HEADLESS)] {
            registered(&daemon, harness, attendance);
            assert_eq!(refusal_of(target_in(&daemon, &sessions, SESSION)), (refusal::CANT_STEER.to_string(), format!("can't steer this session yet: {why}")));
        }
        let plugin_less = cant_steer(NO_MOD);
        assert!(matches!(plugin_less, HostAnswer::Refused { ref code, ref message } if code == refusal::CANT_STEER && message.contains("plugin")), "{plugin_less:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
