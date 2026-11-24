//! `read` (GA04, D3): one page of a session's transcript, about 12 KB,
//! newest first, redacted and framed as foreign text. Claude Code from its
//! registration's transcript path (the file's last 2 MB, through the Claude
//! importer's parser); gruntas-run through the session log reader; Codex
//! refused `cant_read`. The page goes back through the answer record.

use super::super::auto::Config;
use gruntas_client::host_command::HostAnswer;

/// About how much text a page holds (D3).
pub const PAGE_BYTES: usize = 12 * 1024;

/// How much of a Claude Code transcript is read, from its end (D3).
pub const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// The answer to `read { session, page }`: `HostAnswer::Page`, or refused
/// `no_session` or `cant_read`.
pub(crate) fn answer(_c: &Config, _session: &str, _page: Option<&str>) -> HostAnswer {
    super::not_yet("read", "GA04")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA04: a Claude transcript fixture pages newest first, redacted"]
    fn ga04_a_claude_transcript_pages_newest_first_redacted() {
        unimplemented!("GA04")
    }

    #[test]
    #[ignore = "GA04: a gruntas-run session's log reads as pages"]
    fn ga04_a_gruntas_run_log_reads_as_pages() {
        unimplemented!("GA04")
    }

    #[test]
    #[ignore = "GA04: a Codex session is refused cant_read"]
    fn ga04_a_codex_session_cant_be_read_yet() {
        unimplemented!("GA04")
    }
}
