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
//! The daemon frames a message's `text` with its origin as it writes it
//! (`neutral::steer_framed`, the Security model's D7), so the mod submits
//! it as it stands. A line older than `STALE` is pruned, and the mod no
//! longer delivers it: its command was answered by then.
//!
//! A line:
//! `{"type":"message","id":"<command id>","text":"…","origin":{"kind":"agent","sessionId":"…","device":"laptop"},"at":"2026-11-24T10:00:00Z"}`
//! `{"type":"interrupt","id":"<command id>","origin":{…},"at":"…"}`
//! An ack: `{"id":"<command id>","outcome":"delivered"}`.

use super::super::auto::Config;
use gruntas_client::host_command::{self, refusal, HostAnswer};
use gruntas_client::protocol::Origin;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// `text` shown to the person, as a toast and in the status line: the
    /// daemon's own word, the account `/exit` continues on (`attended`).
    Notice { id: String, text: String, at: String },
}

impl Steer {
    /// The host command's id: what the mod delivers once, and acks by.
    pub fn id(&self) -> &str {
        match self {
            Steer::Message { id, .. } | Steer::Interrupt { id, .. } | Steer::Notice { id, .. } => id,
        }
    }

    /// Who sent it; a notice is the daemon's own.
    pub fn origin(&self) -> Option<&Origin> {
        match self {
            Steer::Message { origin, .. } | Steer::Interrupt { origin, .. } => Some(origin),
            Steer::Notice { .. } => None,
        }
    }

    /// When the daemon wrote it, RFC 3339.
    pub fn at(&self) -> &str {
        match self {
            Steer::Message { at, .. } | Steer::Interrupt { at, .. } | Steer::Notice { at, .. } => at,
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

/// How long the daemon waits for the mod's ack: a few of its reads.
pub const ACK_WAIT: Duration = Duration::from_secs(10);

/// How old a line may be and still be delivered; shorter than `ACK_WAIT`,
/// so the mod never delivers a steer whose command was answered
/// `cant_steer`. The mod holds the same bound (`STALE_MS`, hooks/usage.ts).
pub const STALE: Duration = Duration::from_secs(8);

/// How often the daemon looks for the ack.
const ACK_EVERY: Duration = Duration::from_millis(200);

/// One writer of the inboxes at a time: commands answer on threads of
/// their own.
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `s` written to `session`'s inbox, then its ack waited for:
/// `HostAnswer::Delivered`, or refused (`nothing_running`, `cant_steer`
/// when no mod reads the inbox).
pub(crate) fn deliver(c: &Config, session: &str, s: &Steer) -> HostAnswer {
    let Some(daemon) = c.socket.parent() else { return HostAnswer::refused(refusal::CANT_STEER, "this machine's daemon has no directory to keep an inbox in") };
    deliver_in(daemon, session, s, ACK_WAIT)
}

/// `deliver` in the daemon dir `daemon`, waiting `wait` for the ack.
pub(crate) fn deliver_in(daemon: &Path, session: &str, s: &Steer, wait: Duration) -> HostAnswer {
    if !host_command::session_id_shaped(session) {
        return HostAnswer::refused(refusal::NO_SESSION, format!("{:?} is not a session id", session.escape_debug().to_string()));
    }
    if let Err(e) = write(daemon, session, s) {
        return HostAnswer::refused(refusal::CANT_STEER, format!("session {session}'s inbox could not be written: {e}"));
    }
    let deadline = Instant::now() + wait;
    loop {
        if let Some(ack) = acked(&acks_path(daemon, session), s.id()) {
            return match ack.outcome {
                Outcome::Delivered => HostAnswer::Delivered { session: session.to_string() },
                Outcome::NothingRunning => HostAnswer::refused(refusal::NOTHING_RUNNING, format!("nothing is running in session {session}")),
                Outcome::Refused => HostAnswer::refused(refusal::CANT_STEER, format!("session {session}'s gruntas mod refused it: {}", ack.message.as_deref().unwrap_or("no reason given"))),
            };
        }
        if Instant::now() >= deadline {
            return super::run::cant_steer(super::run::NO_MOD);
        }
        std::thread::sleep(ACK_EVERY);
    }
}

/// `text` written to `session`'s inbox as a notice, not waited on: the
/// mod shows it on its next read.
pub fn notify(daemon: &Path, session: &str, text: &str) -> std::io::Result<()> {
    if !host_command::session_id_shaped(session) {
        return Err(std::io::Error::other(format!("{session:?} is not a session id")));
    }
    write(daemon, session, &Steer::Notice { id: gruntas_store::new_id(), text: text.to_string(), at: jiff::Timestamp::now().to_string() })
}

/// `s` appended to `session`'s inbox, a message's text framed with its
/// origin, the lines past `STALE` dropped.
fn write(daemon: &Path, session: &str, s: &Steer) -> std::io::Result<()> {
    let s = match s {
        Steer::Message { id, text, origin, at } => Steer::Message { id: id.clone(), text: crate::neutral::steer_framed(text, origin), origin: origin.clone(), at: at.clone() },
        other => other.clone(),
    };
    let dir = daemon.join(DIR);
    gruntas_api::home::make(&dir).map_err(std::io::Error::other)?;
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let file = path(daemon, session);
    let kept = std::fs::read_to_string(&file).unwrap_or_default();
    let now = jiff::Timestamp::now();
    let fresh = |line: &&str| serde_json::from_str::<Steer>(line).ok().and_then(|s| s.at().parse::<jiff::Timestamp>().ok()).is_some_and(|at| now.duration_since(at) < jiff::SignedDuration::try_from(STALE).unwrap_or_default());
    let mut out: String = kept.lines().filter(fresh).map(|l| format!("{l}\n")).collect();
    out.push_str(&serde_json::to_string(&s).map_err(std::io::Error::other)?);
    out.push('\n');
    crate::daemon::registry::write_private(&dir, &format!("{session}.jsonl"), out.as_bytes())
}

/// The ack of the steer `id` in the acks file `file`, once the mod wrote it.
fn acked(file: &Path, id: &str) -> Option<Ack> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines().filter_map(|l| serde_json::from_str::<Ack>(l).ok()).find(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Attendance, LogBody, LogEvent};
    use gruntas_client::host_command::HostCommand;
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

    const TARGET: &str = "01a0ec7b-0000-7000-8000-0000000000a1";
    const PARENT: &str = "01a0ec7b-0000-7000-8000-0000000000b1";
    const ATTENDED: &str = "01a0ec7b-0000-7000-8000-0000000000c1";
    const UNATTENDED: &str = "01a0ec7b-0000-7000-8000-0000000000c2";
    const ID: &str = "0190f3c1-8a2b-4c4d-9e5f-0a1b2c3d4e5f";
    const ID2: &str = "0190f3c1-8a2b-4c4d-9e5f-0a1b2c3d4e60";

    fn start(sessions: &Path, id: &str, parent: Option<&str>, attendance: Attendance) {
        let dir = sessions.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let body = LogBody::SessionStarted { cwd: "/r".into(), gruntas_version: "t".into(), protocol_version: 1, parent_session_id: None, agent: None, attendance: Some(attendance), started_by_session_id: parent.map(String::from), fenced: false };
        let e = LogEvent { id: id.into(), parent_id: None, session_id: id.into(), time_ms: 1, body };
        std::fs::write(dir.join(crate::log::EVENTS_FILE), format!("{}\n", serde_json::to_string(&e).unwrap())).unwrap();
    }

    /// A home with the target, started by `PARENT`, an attended and an
    /// unattended session; the daemon dir under it.
    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("gruntas-ga05-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let sessions = home.join("sessions");
        start(&sessions, TARGET, Some(PARENT), Attendance::Attended);
        start(&sessions, PARENT, None, Attendance::Unattended);
        start(&sessions, ATTENDED, None, Attendance::Attended);
        start(&sessions, UNATTENDED, None, Attendance::Unattended);
        std::fs::create_dir_all(home.join("daemon")).unwrap();
        home
    }

    fn config(home: &Path) -> Config {
        Config {
            socket: home.join("daemon").join("s.sock"),
            sessions: home.join("sessions"),
            version: "t".into(),
            gruntas_home: None,
            build: Box::new(|_, _| Err(("unbuilt".into(), "no bridges here".into()))),
            policy: Box::new(|| Err("off".into())),
            ready: Box::new(|| Ok(())),
            bridged: Default::default(),
            paid_pause: crate::sync::auto::PAID_PAUSE,
            parked_recheck: crate::sync::auto::PARKED_RECHECK,
            lease_held_wait: crate::sync::auto::LEASE_HELD_WAIT,
            announce: None,
        }
    }

    fn agent(session: &str) -> Origin {
        Origin { kind: OriginKind::Agent, session_id: Some(session.into()), device: Some("laptop".into()), attendance: None }
    }

    /// GA05 (D5, R-GOD-6): each `steer` setting value, checked in the
    /// command dispatch: a refused steer is answered `steer_refused` and
    /// writes nothing; a permitted one reaches the inbox.
    #[test]
    fn ga05_the_inbox_is_written_only_for_a_permitted_steer() {
        let home = home("permit");
        let c = config(&home);
        let inbox = path(&home.join("daemon"), TARGET);
        let cases = [("children", PARENT, true), ("children", ATTENDED, false), ("attended", ATTENDED, true), ("attended", UNATTENDED, false), ("any", UNATTENDED, true)];
        for (setting, from, permitted) in cases {
            std::fs::write(home.join(gruntas_api::home::CONFIG), format!(r#"{{"steer":"{setting}"}}"#)).unwrap();
            let _ = std::fs::remove_file(&inbox);
            let command = HostCommand::Message { session: TARGET.into(), text: "rebase first".into(), origin: agent(from) };
            let answer = super::super::answer(&c, ID, command);
            let refused = matches!(&answer, HostAnswer::Refused { code, .. } if code == refusal::STEER_REFUSED);
            assert_eq!(refused, !permitted, "{setting} from {from}: {answer:?}");
            if permitted {
                // Past the check, the session's kind picks the path (GA06's
                // `run::target`); a Claude Code session's is the inbox.
                crate::steer::allow(crate::steer::read(&c.sessions), Some(&agent(from)), TARGET, &c.sessions).unwrap();
                let s = Steer::Interrupt { id: ID.into(), origin: agent(from), at: jiff::Timestamp::now().to_string() };
                deliver_in(&home.join("daemon"), TARGET, &s, Duration::ZERO);
            }
            assert_eq!(inbox.exists(), permitted, "{setting} from {from}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The mod's ack of the steer `id`, written once it reads it in the inbox.
    fn mod_acks(daemon: PathBuf, id: &'static str, outcome: &'static str) -> std::thread::JoinHandle<Steer> {
        std::thread::spawn(move || {
            for _ in 0..200 {
                let text = std::fs::read_to_string(path(&daemon, TARGET)).unwrap_or_default();
                if let Some(s) = text.lines().map(|l| serde_json::from_str::<Steer>(l).unwrap()).find(|s| s.id() == id) {
                    std::fs::write(acks_path(&daemon, TARGET), format!("{{\"id\":\"other\",\"outcome\":\"delivered\"}}\n{{\"id\":\"{}\",\"outcome\":\"{outcome}\"}}\n", s.id())).unwrap();
                    return s;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("no inbox line")
        })
    }

    /// GA05 (D5): a delivered message's ack answers `delivered`, its text
    /// framed with its origin in the inbox; an interrupt with nothing
    /// running answers `nothing_running`; no ack in time is `cant_steer`.
    #[test]
    fn ga05_the_mods_ack_answers_the_command() {
        let home = home("ack");
        let daemon = home.join("daemon");
        let acker = mod_acks(daemon.clone(), ID, "delivered");
        let m = Steer::Message { id: ID.into(), text: "rebase first".into(), origin: agent(ATTENDED), at: jiff::Timestamp::now().to_string() };
        assert_eq!(deliver_in(&daemon, TARGET, &m, Duration::from_secs(5)), HostAnswer::Delivered { session: TARGET.into() });
        let Steer::Message { text, .. } = acker.join().unwrap() else { panic!("not the message") };
        assert_eq!(text, format!("<steer from {ATTENDED} on laptop, by an agent>\nrebase first\n</steer>"));

        let _ = std::fs::remove_file(acks_path(&daemon, TARGET));
        let acker = mod_acks(daemon.clone(), ID2, "nothingRunning");
        let i = Steer::Interrupt { id: ID2.into(), origin: agent(ATTENDED), at: jiff::Timestamp::now().to_string() };
        let answer = deliver_in(&daemon, TARGET, &i, Duration::from_secs(5));
        acker.join().unwrap();
        assert!(matches!(&answer, HostAnswer::Refused { code, .. } if code == refusal::NOTHING_RUNNING), "{answer:?}");
        // Both lines still fresh: the mod has each, by id.
        assert_eq!(std::fs::read_to_string(path(&daemon, TARGET)).unwrap().lines().count(), 2);

        let _ = std::fs::remove_file(acks_path(&daemon, TARGET));
        let silent = Steer::Interrupt { id: "0190f3c1-8a2b-4c4d-9e5f-0a1b2c3d4e61".into(), origin: agent(ATTENDED), at: "2026-01-01T00:00:00Z".into() };
        let answer = deliver_in(&daemon, TARGET, &silent, Duration::ZERO);
        assert!(matches!(&answer, HostAnswer::Refused { code, .. } if code == refusal::CANT_STEER), "{answer:?}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
