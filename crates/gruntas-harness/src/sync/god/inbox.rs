//! An interactive Claude Code session's steer inbox (GA05, D5).
//!
//! The daemon appends each permitted steer as one JSON line to
//! `<daemon dir>/inbox/<session id>.jsonl` (`path`), the daemon dir being
//! its runtime directory (`crate::daemon::dir`, where its socket is). The
//! plugin's mod reads that file every `POLL` with `$.fs.read` (no process
//! spawned), delivers each line it has not delivered once, by `id`: a
//! message with `$.prompt.submit`, framed with its origin; an interrupt
//! with `$.turn.abort` on the live turn it tracks. It says what came of
//! each as one JSON line of `Ack` in `<session id>.acks.jsonl` beside it
//! (`acks_path`), which the daemon waits on to answer the command:
//! `delivered`, or `nothingRunning` for an interrupt with no turn.
//!
//! A line:
//! `{"type":"message","id":"<command id>","text":"…","origin":{"kind":"agent","sessionId":"…","device":"laptop"},"at":"2026-11-24T10:00:00Z"}`
//! `{"type":"interrupt","id":"<command id>","origin":{…},"at":"…"}`
//! An ack: `{"id":"<command id>","outcome":"delivered"}`.

use super::super::auto::Config;
use gruntas_client::host_command::HostAnswer;
use gruntas_client::protocol::Origin;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The inboxes' directory, in the daemon's.
pub const DIR: &str = "inbox";

/// How often the mod reads its inbox (D5).
pub const POLL: Duration = Duration::from_secs(2);

/// One steer, as a line of the inbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Steer {
    /// `text` submitted as a prompt, framed with `origin`.
    Message { id: String, text: String, origin: Origin, at: String },
    /// The running turn aborted.
    Interrupt { id: String, origin: Origin, at: String },
}

impl Steer {
    /// The host command's id: what the mod delivers once, and acks by.
    pub fn id(&self) -> &str {
        match self {
            Steer::Message { id, .. } | Steer::Interrupt { id, .. } => id,
        }
    }

    pub fn origin(&self) -> &Origin {
        match self {
            Steer::Message { origin, .. } | Steer::Interrupt { origin, .. } => origin,
        }
    }
}

/// What came of a steer, as the mod says it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ack {
    pub id: String,
    pub outcome: Outcome,
    /// The mod's words, when it refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Delivered,
    /// An interrupt with no turn running.
    NothingRunning,
    /// Anything else the mod would not do; `Ack::message` says why.
    Refused,
}

/// `session`'s inbox in the daemon dir `daemon`. `session` is a session id
/// (`host_command::session_id_shaped`), held so as the command opened.
pub fn path(daemon: &Path, session: &str) -> PathBuf {
    daemon.join(DIR).join(format!("{session}.jsonl"))
}

/// Where the mod acks `session`'s steers.
pub fn acks_path(daemon: &Path, session: &str) -> PathBuf {
    daemon.join(DIR).join(format!("{session}.acks.jsonl"))
}

/// `s` written to `session`'s inbox, then its ack waited for:
/// `HostAnswer::Delivered`, or refused (`nothing_running`, `cant_steer`
/// when no mod reads the inbox).
pub(crate) fn deliver(_c: &Config, _session: &str, _s: &Steer) -> HostAnswer {
    super::not_yet("a steer of a Claude Code session", "GA05")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::protocol::OriginKind;

    #[test]
    fn ga01_an_inbox_line_and_its_ack_read_as_the_mod_reads_them() {
        let origin = Origin { kind: OriginKind::Agent, session_id: Some("01a0ec7b-0000-7000-8000-0000000000a1".into()), device: Some("laptop".into()), attendance: None };
        let m = Steer::Message { id: "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e5f".into(), text: "rebase first".into(), origin: origin.clone(), at: "2026-11-24T10:00:00Z".into() };
        let line = serde_json::to_string(&m).unwrap();
        assert_eq!(line, r#"{"type":"message","id":"0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e5f","text":"rebase first","origin":{"kind":"agent","sessionId":"01a0ec7b-0000-7000-8000-0000000000a1","device":"laptop"},"at":"2026-11-24T10:00:00Z"}"#);
        assert_eq!(serde_json::from_str::<Steer>(&line).unwrap(), m);
        let i: Steer = serde_json::from_str(r#"{"type":"interrupt","id":"x","origin":{"kind":"agent"},"at":"t"}"#).unwrap();
        assert_eq!(i.id(), "x");
        let ack: Ack = serde_json::from_str(r#"{"id":"x","outcome":"nothingRunning"}"#).unwrap();
        assert_eq!(ack.outcome, Outcome::NothingRunning);
        let d = Path::new("/run/gruntas/abc");
        assert_eq!((path(d, "s"), acks_path(d, "s")), (d.join("inbox/s.jsonl"), d.join("inbox/s.acks.jsonl")));
    }

    #[test]
    #[ignore = "GA05: steers permitted and refused per steer setting value; the inbox written only when permitted"]
    fn ga05_the_inbox_is_written_only_for_a_permitted_steer() {
        unimplemented!("GA05")
    }

    #[test]
    #[ignore = "GA05: a delivered steer's ack answers the command delivered; nothing running answers nothing_running"]
    fn ga05_the_mods_ack_answers_the_command() {
        unimplemented!("GA05")
    }
}
