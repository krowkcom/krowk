//! The god agent's host commands, as the target daemon answers them (god
//! agent spec, D3–D5): `read`, `changes`, `message` and `interrupt`,
//! delivered as every host command is (`super::commands`) and answered here
//! on the command's own thread. An answer too big for 4 KB goes in the
//! command's answer record on its way out; nothing here knows.
//!
//! `message` and `interrupt` are steering: their origin is checked against
//! the `steer` setting here, the one place both delivery paths go through,
//! before the session is looked at. Then the session's kind picks the path
//! (`run::target`): an interactive Claude Code session through its inbox
//! (`inbox`), a gruntas-run session through the daemon (`run`), any other
//! refused `cant_steer` with the reason.
//!
//! Each slice lives in its own file: `changes` (GA03), `read` (GA04),
//! `inbox` (GA05), `run` (GA06).

use super::announce::Announcer;
use super::auto::Config;
use gruntas_client::e2e::DeviceId;
use gruntas_client::host_command::{self, refusal, HostAnswer, HostCommand};
use std::time::{Duration, Instant};

pub mod changes;
pub mod inbox;
pub mod read;
pub mod run;

/// How a session is steered, once the `steer` setting let the origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// An interactive Claude Code session with the plugin's mod: its inbox.
    ClaudeCode,
    /// A session gruntas runs: the daemon's own prompt, steer and interrupt.
    GruntasRun,
}

/// The answer to one of the god agent's commands, `id` the command's.
pub(crate) fn answer(c: &Config, id: &str, command: HostCommand) -> HostAnswer {
    match command {
        HostCommand::Changes { since } => changes::answer(c, &since),
        HostCommand::Read { session, page } => read::answer(c, &session, page.as_deref()),
        HostCommand::Message { session, text, origin } => steer(c, &session, inbox::Steer::Message { id: id.to_string(), text, origin, at: now() }),
        HostCommand::Interrupt { session, origin } => steer(c, &session, inbox::Steer::Interrupt { id: id.to_string(), origin, at: now() }),
        _ => HostAnswer::refused("bad_command", "not one of the god agent's commands"),
    }
}

/// A steer of `session`: the `steer` setting first, then the path its kind
/// takes.
fn steer(c: &Config, session: &str, s: inbox::Steer) -> HostAnswer {
    if let Err(why) = crate::steer::allow(crate::steer::read(&c.sessions), Some(s.origin()), session, &c.sessions) {
        return HostAnswer::refused(refusal::STEER_REFUSED, why);
    }
    match run::target(c, session) {
        Ok(Target::ClaudeCode) => inbox::deliver(c, session, &s),
        Ok(Target::GruntasRun) => run::deliver(c, session, &s),
        Err(refused) => refused,
    }
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

/// How long the asking side waits for an answer: the target's presence
/// refresh (up to 10 s, D8) and the work, with room.
pub const ASK_WAIT: Duration = Duration::from_secs(30);
/// How often it looks.
const ASK_EVERY: Duration = Duration::from_millis(250);

/// `command` asked of the machine `to`, called `name`, by `a`'s device, and
/// its answer, opened, waited for up to `wait`. A `recorded` answer is
/// followed to its answer record. `Err` is `(code, message)`:
/// `host_outdated` for a host older than the command ("that machine needs
/// updating"), `host_silent` for none in time, `bad_answer`, or the
/// registry's code.
pub fn ask(a: &Announcer, to: &DeviceId, name: &str, command: &HostCommand, wait: Duration) -> Result<HostAnswer, (String, String)> {
    let target = to.to_string();
    let id = super::fleet::command_id();
    let sealed = host_command::seal_command(command, &id, &a.device, to, &a.keys, a.chain.generation()).map_err(|e| ("seal_failed".to_string(), e))?;
    let registry = |e: gruntas_api::Error| (e.code(), format!("{name} could not be asked: {}", e.fix()));
    a.api.send_host_command(&target, &id, &sealed).map_err(registry)?;
    let bad = |e: String| ("bad_answer".to_string(), e);
    let deadline = Instant::now() + wait;
    let answer = loop {
        let s = a.api.host_command(&target, &id).map_err(registry)?;
        if s.answered {
            let sealed = s.answer.ok_or_else(|| bad(format!("{name} answered with nothing this gruntas reads")))?;
            break host_command::open_answer(&sealed, &id, to, &a.device, &a.keys, &a.chain).map_err(bad)?;
        }
        if Instant::now() >= deadline {
            return Err(("host_silent".into(), format!("{name} didn't answer within {} s", wait.as_secs())));
        }
        std::thread::sleep(ASK_EVERY);
    };
    let answer = match answer {
        HostAnswer::Recorded => {
            let record = a.api.answer_record(&target, &id).map_err(registry)?.ok_or_else(|| bad(format!("{name} answered in a record that is not there")))?;
            host_command::open_record(&record.sealed, &id, to, &a.device, &a.keys, &a.chain).map_err(bad)?
        }
        answer => answer,
    };
    match answer.refusal_line(name) {
        Some(line) if answer.needs_updating() => Err(("host_outdated".into(), line)),
        _ => Ok(answer),
    }
}
