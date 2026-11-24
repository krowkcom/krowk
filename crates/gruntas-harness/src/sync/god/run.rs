//! Steering a gruntas-run session, and telling the kinds apart (GA06, D5):
//! a message is a prompt (with its origin) when the session is idle and a
//! steer when a turn runs; an interrupt is today's interrupt command. A
//! Codex, headless or plugin-less session is refused `cant_steer`, "can't
//! steer this session yet", with the reason.

use super::super::auto::Config;
use super::inbox::Steer;
use super::Target;
use gruntas_client::host_command::HostAnswer;

/// Which path steers `session`, or the refusal: `no_session` when this
/// machine holds none by that id, `cant_steer` with the reason for a kind
/// that can't be steered yet.
pub(crate) fn target(_c: &Config, _session: &str) -> Result<Target, HostAnswer> {
    Err(super::not_yet("message or interrupt", "GA06"))
}

/// `s` given to the gruntas-run session `session`: `HostAnswer::Delivered`,
/// or refused (`nothing_running` for an interrupt with no turn).
pub(crate) fn deliver(_c: &Config, _session: &str, _s: &Steer) -> HostAnswer {
    super::not_yet("a steer of a gruntas-run session", "GA06")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA06: an idle gruntas-run session gets a message as a prompt with its origin"]
    fn ga06_an_idle_session_gets_a_message_as_a_prompt() {
        unimplemented!("GA06")
    }

    #[test]
    #[ignore = "GA06: a running gruntas-run session gets a message as a steer"]
    fn ga06_a_running_session_gets_a_message_as_a_steer() {
        unimplemented!("GA06")
    }

    #[test]
    #[ignore = "GA06: an interrupt stops a gruntas-run session's turn"]
    fn ga06_an_interrupt_stops_the_turn() {
        unimplemented!("GA06")
    }

    #[test]
    #[ignore = "GA06: Codex, headless and plugin-less sessions are refused cant_steer with the reason"]
    fn ga06_other_sessions_are_refused_with_the_reason() {
        unimplemented!("GA06")
    }
}
