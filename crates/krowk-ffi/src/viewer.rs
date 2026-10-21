//! The live viewer of a synced session, as the phone drives it (R-CLIENT-1,
//! R-HAND-4, R-IOS-5): `krowk_client::sync::viewer::ViewerCore` and, while
//! a join is in progress, `krowk_client::sync::join::Joining`, behind one
//! mutex. Swift owns the WebSocket (Network.framework) and the timers; it
//! passes in what its socket and clock observe and carries out the
//! [`Effect`]s it gets back. Every decision — the handshake, the welcome,
//! the order batches apply in, gaps and catch-ups, the heartbeat, the
//! commands' ids, queue and acks — is the core's.
//!
//! Every method locks, calls, converts and returns: no thread, no runtime,
//! no socket, no sleep, and the time is always the caller's `now_ms`. The
//! one blocking call, [`Viewer::read_chunks`], copies what it needs under
//! the lock and reads the registry after letting it go. Relay only: the
//! direct paths (`Probe`, `Leave`, `move_to`) are not the phone's in M1,
//! and a race the core asks for is lost at once, here.

use std::sync::{Arc, Mutex};

use krowk_client::e2e::RELAY_ROLE_VIEWER;
use krowk_client::protocol::{Command, LogEvent, StreamLine};
use krowk_client::sync::join::{self, Joining, Step};
use krowk_client::sync::store::{self, Attached, Head};
use krowk_client::sync::viewer::{self as core, Update, ViewerCore};
use serde::Deserialize;
use serde_json::Value;

use crate::KrowkError;
use crate::sessions::{HeadRecord, OpenedSession};

/// What Swift does next, in the order given.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Effect {
    /// At `at_ms` (now, if it has passed), fetch a relay ticket with
    /// `OpenedSession::relay_ticket` and hand it to `ticket`, or why it
    /// failed to `ticket_failed`. A later `NeedTicket` replaces one not yet
    /// acted on.
    NeedTicket { at_ms: u64 },
    /// Open a WebSocket to `url`, `ticket` in its `X-Krowk-Ticket` header
    /// (never in the URL), and pass what it receives to `received` and its
    /// end to `closed`.
    Dial { url: String, ticket: String },
    /// One binary message on the current connection.
    Send { bytes: Vec<u8> },
    /// The text message `ping` on the current connection.
    Ping,
    /// Drop the current connection, and pass nothing more from it, its
    /// close included: a `NeedTicket` follows.
    Close,
    /// Updates for the screen, at most one `Show` per display frame.
    Show { updates: Vec<ViewerUpdate> },
    /// Call `read_chunks` off the main thread, then `chunks_read` with what
    /// it returns. Until then the viewer holds the stream's batches.
    ReadChunks,
    /// The viewer cannot go on; the `Show` before it says why. Close the
    /// connection and call nothing more.
    End,
}

/// One message the current connection received.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum WsFrame {
    /// A binary message: the relay's handshake or control, or an envelope.
    Binary { bytes: Vec<u8> },
    /// A text message (the relay's `pong`): the connection lives.
    Text { text: String },
}

/// What the screen draws from: `krowk_client::sync::viewer::Update`, every
/// variant of it, its events decoded.
// A line is most of what a viewer is handed, so it stays unboxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ViewerUpdate {
    /// The session as the chunks hold it, once, as the viewer attaches.
    Attached { events: Vec<LogEvent>, head: Option<HeadRecord> },
    Line { line: StreamLine },
    /// A host is on the channel and welcomed this viewer, or none is: the
    /// phone is read-only until one is, and commands queue (R-HAND-4).
    Host { present: bool },
    /// A command, shown at once (R-IOS-5); `queued` when no host is there
    /// to take it yet. `id` is `<device>-<run id>-<counter>`.
    Sent { id: String, queued: bool },
    /// The host took the command `id`, or refused it with `error`.
    Acked { id: String, error: Option<String> },
    /// Logged events the live stream did not bring: from the chunks or the
    /// host's catch-up.
    CaughtUp { events: Vec<LogEvent> },
    /// The path the session comes by: `relay` on the phone.
    Path { path: String, via: Option<String> },
    /// The relay dropped a batch: the host is asked for what it held, and
    /// `CaughtUp` follows.
    Gap,
    /// Something the viewer cannot go on from, or events this build cannot
    /// decode (a newer host's: the app may need updating).
    Failed { detail: String },
    /// Something to tell the person: why the relay cannot be joined, said
    /// once as it comes or changes.
    Note { text: String },
    /// A handoff to this device went through. Never on the phone, which
    /// never becomes a host (R-HAND-4); what it carries stays in Rust.
    HandoffTaken,
    /// A handoff to this device was refused, and why.
    HandoffRefused { detail: String },
}

impl From<Update> for ViewerUpdate {
    // No `_` arm: a new `Update` fails this build until it crosses.
    fn from(u: Update) -> ViewerUpdate {
        match u {
            Update::Attached { events, head } => match decode(&events) {
                Ok(events) => ViewerUpdate::Attached { events, head: head.map(Into::into) },
                Err(detail) => ViewerUpdate::Failed { detail },
            },
            Update::Line(line) => ViewerUpdate::Line { line },
            Update::Host(present) => ViewerUpdate::Host { present },
            Update::Sent { id, queued } => ViewerUpdate::Sent { id, queued },
            Update::Acked { id, error } => ViewerUpdate::Acked { id, error },
            Update::CaughtUp(events) => match decode(&events) {
                Ok(events) => ViewerUpdate::CaughtUp { events },
                Err(detail) => ViewerUpdate::Failed { detail },
            },
            Update::Path { path, via } => ViewerUpdate::Path { path, via },
            Update::Gap => ViewerUpdate::Gap,
            Update::Failed(detail) => ViewerUpdate::Failed { detail },
            Update::Note(text) => ViewerUpdate::Note { text },
            Update::Handoff(Ok(_)) => ViewerUpdate::HandoffTaken,
            Update::Handoff(Err(detail)) => ViewerUpdate::HandoffRefused { detail },
        }
    }
}

/// Every event decoded, or why the first that does not decode failed.
fn decode(events: &[Value]) -> Result<Vec<LogEvent>, String> {
    events
        .iter()
        .map(|v| {
            LogEvent::deserialize(v).map_err(|e| {
                let id = v["id"].as_str().unwrap_or("with no id");
                format!("event {id} is not a log event this build understands — the app may need updating: {e}")
            })
        })
        .collect()
}

/// What `ViewerCore::chunks_read` takes: the events read and where the
/// reading stopped, or why it failed.
type Read = Result<(Vec<Value>, Attached), String>;

/// The chunks one `ReadChunks` read, or why the read failed: opaque, handed
/// back to the `Viewer` that asked, once.
#[derive(uniffi::Object)]
pub struct ChunksRead {
    /// Which `ReadChunks` it answers: one answering an earlier one is
    /// dropped.
    asked: u64,
    read: Mutex<Option<Read>>,
}

/// A synced session followed live through the relay. Swift calls it from
/// one serial queue; the mutex is for safety, not parallelism.
#[derive(uniffi::Object)]
pub struct Viewer {
    inner: Mutex<Inner>,
}

struct Inner {
    core: ViewerCore,
    opened: Arc<OpenedSession>,
    /// `ViewerCore::new`'s effects, until `start`.
    first: Option<Vec<core::Effect>>,
    /// The `extra` of the join asked for and not yet dialed.
    pending: Option<Value>,
    /// The join dialed and not yet answered `joined`.
    joining: Option<Joining>,
    /// Whether Swift holds a connection this viewer dialed and has not
    /// been told to close, or told closed.
    dialed: bool,
    /// The `ReadChunks` out, by number, and the head it reads to.
    asked: u64,
    want: Option<Head>,
}

/// What a viewer whose lock was poisoned says, every time.
fn poisoned() -> Vec<Effect> {
    vec![Effect::Show { updates: vec![ViewerUpdate::Failed { detail: "the viewer failed inside and cannot go on — open the session again".into() }] }, Effect::End]
}

/// Locks and runs `f`; a poisoned lock says the viewer failed, and ends,
/// rather than panicking across the FFI.
fn locked<T>(m: &Mutex<T>, f: impl FnOnce(&mut T) -> Vec<Effect>) -> Vec<Effect> {
    match m.lock() {
        Ok(mut i) => f(&mut i),
        Err(_) => poisoned(),
    }
}

#[uniffi::export]
impl Viewer {
    /// A viewer of `session` as it was opened and read so far, joining the
    /// relay `Account` chose for it. Nothing happens until `start`. Waits
    /// for a `catch_up` of the session's that is running; never touches
    /// the network itself.
    #[uniffi::constructor]
    pub fn open(session: Arc<OpenedSession>, now_ms: u64) -> Result<Arc<Viewer>, KrowkError> {
        let at_rest = session.snapshot();
        let (core, first) = ViewerCore::new(session.keys().clone(), &session.id(), session.device(), session.relay(), at_rest, now_ms);
        let inner = Inner { core, opened: session, first: Some(first), pending: None, joining: None, dialed: false, asked: 0, want: None };
        Ok(Arc::new(Viewer { inner: Mutex::new(inner) }))
    }

    /// The first effects: the session as read at rest (`Show` with
    /// `Attached`), and the first `NeedTicket`. Empty if called again.
    pub fn start(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let first = i.first.take().unwrap_or_default();
            i.effects(first, now_ms)
        })
    }

    /// The relay ticket a `NeedTicket` asked for: the `Dial`, and the join
    /// it carries out once the relay's challenge comes. Nothing if no join
    /// is waiting for one.
    pub fn ticket(&self, ticket: String, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let Some(extra) = i.pending.take() else { return Vec::new() };
            let o = &i.opened;
            let id = o.id();
            match Joining::new(o.relay(), &id, o.env(), o.device(), RELAY_ROLE_VIEWER, extra) {
                Ok(j) => {
                    let d = join::dial(o.relay(), &id, o.env(), &ticket);
                    i.joining = Some(j);
                    i.dialed = true;
                    vec![Effect::Dial { url: d.url, ticket: d.ticket }]
                }
                Err(e) => {
                    let out = i.core.join_failed(e, now_ms);
                    i.effects(out, now_ms)
                }
            }
        })
    }

    /// The ticket a `NeedTicket` asked for could not be had, and why: said
    /// to the person, and asked for again later.
    pub fn ticket_failed(&self, detail: String, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if i.pending.take().is_none() {
                return Vec::new();
            }
            let out = i.core.join_failed(detail, now_ms);
            i.effects(out, now_ms)
        })
    }

    /// A message the current connection received.
    pub fn received(&self, frame: WsFrame, now_ms: u64) -> Vec<Effect> {
        self.with(|i| match frame {
            WsFrame::Text { .. } => {
                i.core.alive(now_ms);
                Vec::new()
            }
            WsFrame::Binary { bytes } => {
                let Some(j) = i.joining.as_mut() else {
                    let out = i.core.received(&bytes, now_ms);
                    return i.effects(out, now_ms);
                };
                let step = j.message(i.opened.signing(), &bytes);
                let out = match step {
                    Ok(Step::Wait) => return Vec::new(),
                    Ok(Step::Send(bytes)) => return vec![Effect::Send { bytes }],
                    Ok(Step::Joined(v)) => {
                        i.joining = None;
                        i.core.joined(&v, now_ms)
                    }
                    Err(e) => {
                        i.joining = None;
                        i.core.join_failed(e, now_ms)
                    }
                };
                i.effects(out, now_ms)
            }
        })
    }

    /// The current connection ended, or failed to open.
    pub fn closed(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            i.dialed = false;
            let out = match i.joining.take() {
                Some(_) => i.core.join_failed("the connection to the relay ended before it answered the join".into(), now_ms),
                None => i.core.closed(now_ms),
            };
            i.effects(out, now_ms)
        })
    }

    /// The time passing: call it when `next_wake_ms` says.
    pub fn tick(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let out = i.core.tick(now_ms);
            i.effects(out, now_ms)
        })
    }

    /// A command of the person's: shown at once as `Sent`, sent once a host
    /// has welcomed this viewer and queued until then (R-HAND-4, R-IOS-5).
    pub fn command(&self, command: Command, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let out = i.core.command(command, now_ms);
            i.effects(out, now_ms)
        })
    }

    /// When to call `tick` next, which may be past already: then at once.
    /// None once ended. Ask again after every call.
    pub fn next_wake_ms(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|i| i.core.next_wake())
    }

    /// Reads the chunks the host wrote since the viewer's last read, up to
    /// the head its welcome named, for `chunks_read`. A failed read is in
    /// what it returns too: hand it on all the same. Blocking: a round trip
    /// or more to the registry, with the viewer's lock not held; call it
    /// off the main thread, after a `ReadChunks`.
    pub fn read_chunks(&self) -> Arc<ChunksRead> {
        let copied = self.inner.lock().map(|i| (i.asked, i.want, i.core.attached().clone(), Arc::clone(&i.opened)));
        let (asked, read) = match copied {
            Ok((asked, want, mut a, o)) => (asked, store::catch_up(o.client(), o.keys(), &o.id(), &mut a, want).map(|fresh| (fresh, a))),
            Err(_) => (0, Err("the viewer failed inside".into())),
        };
        Arc::new(ChunksRead { asked, read: Mutex::new(Some(read)) })
    }

    /// What `read_chunks` returned. One answering an earlier `ReadChunks`
    /// than the last, or handed in twice, is dropped.
    pub fn chunks_read(&self, read: Arc<ChunksRead>, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if read.asked != i.asked {
                return Vec::new();
            }
            let Some(r) = read.read.lock().ok().and_then(|mut r| r.take()) else { return Vec::new() };
            let out = i.core.chunks_read(r, now_ms);
            i.effects(out, now_ms)
        })
    }
}

impl Viewer {
    fn with(&self, f: impl FnOnce(&mut Inner) -> Vec<Effect>) -> Vec<Effect> {
        locked(&self.inner, f)
    }
}

impl Inner {
    /// The core's effects as Swift carries them out. No `_` arm: a new
    /// `core::Effect` fails this build until it is handled.
    fn effects(&mut self, from: Vec<core::Effect>, now_ms: u64) -> Vec<Effect> {
        let mut out = Vec::with_capacity(from.len());
        for e in from {
            match e {
                core::Effect::Send(bytes) => out.push(Effect::Send { bytes }),
                core::Effect::Ping => out.push(Effect::Ping),
                core::Effect::Show(updates) => out.push(Effect::Show { updates: updates.into_iter().map(Into::into).collect() }),
                core::Effect::ReadChunks { want } => {
                    self.asked += 1;
                    self.want = want;
                    out.push(Effect::ReadChunks);
                }
                core::Effect::Join { extra, at_ms } => {
                    if std::mem::take(&mut self.dialed) {
                        out.push(Effect::Close);
                    }
                    self.joining = None;
                    self.pending = Some(extra);
                    out.push(Effect::NeedTicket { at_ms });
                }
                // Relay only on the phone: a race for a direct path is lost
                // as it is asked, and the core asks again later, backing off.
                core::Effect::Probe => self.core.probe_lost(now_ms),
                // Only a move to a direct path leaves a connection behind.
                core::Effect::Leave => {}
                core::Effect::End => {
                    self.pending = None;
                    self.joining = None;
                    self.dialed = false;
                    out.push(Effect::End);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // The wrapper's own bookkeeping; the protocol is tests/viewer.rs's.
    use super::*;

    #[test]
    fn r_client_1_an_update_that_does_not_decode_is_failed() {
        let u: ViewerUpdate = Update::CaughtUp(vec![serde_json::json!({"id": "x", "type": "turn.teleported"})]).into();
        assert!(matches!(u, ViewerUpdate::Failed { detail } if detail.contains("event x")));
        let u: ViewerUpdate = Update::Handoff(Err("no".into())).into();
        assert!(matches!(u, ViewerUpdate::HandoffRefused { detail } if detail == "no"));
    }

    #[test]
    fn r_client_1_a_poisoned_viewer_says_it_failed_and_ends() {
        let m = Mutex::new(0);
        let _ = std::thread::scope(|t| t.spawn(|| locked(&m, |_| panic!("inside"))).join());
        assert!(m.is_poisoned());
        match locked(&m, |_| unreachable!()).as_slice() {
            [Effect::Show { updates }, Effect::End] => assert!(matches!(updates.as_slice(), [ViewerUpdate::Failed { .. }])),
            other => panic!("{other:?}"),
        }
    }
}
