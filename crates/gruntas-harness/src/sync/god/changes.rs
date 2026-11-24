//! `changes` (GA03, D4): what this machine's sessions did since a time, from
//! the daemon's 24-hour log of their state transitions and reports.

use super::super::auto::Config;
use gruntas_client::host_command::HostAnswer;

/// The answer to `changes { since }`: `HostAnswer::Changes`, oldest first.
pub(crate) fn answer(_c: &Config, _since: &str) -> HostAnswer {
    super::not_yet("changes", "GA03")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA03: the daemon keeps 24 hours of transitions and reports, and answers those since a time"]
    fn ga03_changes_since_a_time_are_its_transitions_and_reports() {
        unimplemented!("GA03")
    }
}
