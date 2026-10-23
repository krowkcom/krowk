//! The live viewer of a synced session, as the phone drives it (R-CLIENT-1,
//! R-HAND-4, R-IOS-5): `krowk_client::sync::viewer::ViewerCore` and, while
//! a join is in progress, `krowk_client::sync::join::Joining`, behind one
//! mutex. Swift owns the WebSocket (Network.framework) and the timers; it
//! passes in what its socket and clock observe and carries out the
//! [`Effect`]s it gets back. Every decision — the handshake and how long it
//! may take, the welcome, the order batches apply in, gaps and catch-ups,
//! the heartbeat, the commands' ids, queue and acks — is Rust's.
//!
//! Every method locks, calls, converts and returns: no thread, no runtime,
//! no socket, no sleep, and the time is always the caller's `now_ms`. The
//! one blocking call, [`Viewer::read_chunks`], copies what it needs under
//! the lock and reads the registry after letting it go. Relay only: the
//! direct paths (`Probe`, `Leave`, `move_to`) are not the phone's in M1,
//! and a race the core asks for is lost at once, here.
//!
//! Each connection `Dial` asks for is numbered, and what Swift passes in
//! names the connection it came from: anything from one this viewer has
//! since dropped (`Close`) — a message or a close Swift's queue still held
//! — is ignored, so it never reaches the join or the session that follows.
//!
//! When it joins again is its `ConnectionSupervisor`'s (ticket 86): a loss
//! waits on a jittered backoff from 250 ms to 30 s rather than the core's
//! fixed second, a loss in the background waits for the foreground, and
//! `wake`, `foreground` and `background` carry the app's lifecycle and
//! network changes in. The join itself still resumes at the core's cursor.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use krowk_client::e2e::{self, RELAY_ROLE_VIEWER};
use krowk_client::protocol::{Command, LogEvent, StreamLine};
use krowk_client::sync::join::{self, Joining, Step};
use krowk_client::sync::store::{self, Attached, Head};
use krowk_client::sync::viewer::{self as core, Update, ViewerCore};
use serde::Deserialize;
use serde_json::Value;

use crate::KrowkError;
use crate::sessions::{HeadRecord, OpenedSession};
use crate::supervisor::{ConnectionSupervisor, LinkState, WakeAction};

/// How long the relay has from the dial to its first message, and then to
/// send each next message of the join (the terminal's `JOIN_WAIT`, which
/// counts the connect on its own): past it the join failed, and is tried
/// again.
const JOIN_WAIT_MS: u64 = 10_000;

/// What Swift does next, in the order given.
#[derive(Clone, uniffi::Enum)]
pub enum Effect {
    /// At `at_ms` (now, if it has passed), fetch a relay ticket with
    /// `OpenedSession::relay_ticket`, off the main thread, and hand it to
    /// `ticket`, or why it failed to `ticket_failed`. A later `NeedTicket`
    /// replaces one not yet acted on. Later than now, it is the
    /// supervisor's backoff: the screen may say when.
    NeedTicket { at_ms: u64 },
    /// Open WebSocket `connection` to `url`, `ticket` in its
    /// `X-Krowk-Ticket` header (never in the URL), and pass each message it
    /// receives to `received` and its end, or its failure to open, to
    /// `closed`, each with `connection`. The join's waits are the viewer's:
    /// keep calling `tick` at `next_wake_ms`.
    Dial { url: String, ticket: String, connection: u64 },
    /// One binary message on the current connection; with none open
    /// (it closed while a `ReadChunks` was out), drop it.
    Send { bytes: Vec<u8> },
    /// The text message `ping` on the current connection; with none open,
    /// drop it.
    Ping,
    /// Drop the current connection; anything more from it is ignored, its
    /// close included. A `NeedTicket` follows.
    Close,
    /// Updates for the screen, at most one `Show` per display frame.
    Show { updates: Vec<ViewerUpdate> },
    /// Call `read_chunks` off the main thread, then `chunks_read` with what
    /// it returns, failed or not. Until then the viewer holds the stream's
    /// batches.
    ReadChunks,
    /// The viewer cannot go on; the `Show` before it says why. Drop the
    /// connection and call nothing more.
    End,
}

// By hand, so a `{:?}` of a `Dial` never logs the relay ticket.
impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::NeedTicket { at_ms } => f.debug_struct("NeedTicket").field("at_ms", at_ms).finish(),
            Effect::Dial { url, connection, .. } => f.debug_struct("Dial").field("url", url).field("ticket", &"…").field("connection", connection).finish(),
            Effect::Send { bytes } => f.debug_struct("Send").field("bytes", &bytes.len()).finish(),
            Effect::Ping => f.write_str("Ping"),
            Effect::Close => f.write_str("Close"),
            Effect::Show { updates } => f.debug_struct("Show").field("updates", updates).finish(),
            Effect::ReadChunks => f.write_str("ReadChunks"),
            Effect::End => f.write_str("End"),
        }
    }
}

/// One message a connection received.
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
    /// Something went wrong. With an `End` after it the viewer cannot go
    /// on; without one, the update before it left out events this build
    /// cannot decode (a newer host's: the app may need updating), and the
    /// viewer goes on.
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

/// The updates one core `Update` crosses as: one, and a `Failed` after it
/// when some of its events do not decode. No `_` arm: a new `Update` fails
/// this build until it crosses.
fn updates(u: Update, out: &mut Vec<ViewerUpdate>) {
    let crossed = match u {
        Update::Attached { events, head } => {
            let (events, failed) = decode(&events);
            out.push(ViewerUpdate::Attached { events, head: head.map(Into::into) });
            failed
        }
        Update::Line(line) => {
            out.push(ViewerUpdate::Line { line });
            None
        }
        Update::Host(present) => {
            out.push(ViewerUpdate::Host { present });
            None
        }
        Update::Sent { id, queued } => {
            out.push(ViewerUpdate::Sent { id, queued });
            None
        }
        Update::Acked { id, error } => {
            out.push(ViewerUpdate::Acked { id, error });
            None
        }
        Update::CaughtUp(events) => {
            let (events, failed) = decode(&events);
            if !events.is_empty() {
                out.push(ViewerUpdate::CaughtUp { events });
            }
            failed
        }
        Update::Path { path, via } => {
            out.push(ViewerUpdate::Path { path, via });
            None
        }
        Update::Gap => {
            out.push(ViewerUpdate::Gap);
            None
        }
        Update::Failed(detail) => {
            out.push(ViewerUpdate::Failed { detail });
            None
        }
        // The core says it tries again every second; the supervisor's
        // backoff times the phone's retries, and the screen says when.
        Update::Note(text) => {
            let text = match text.strip_suffix(core::EVERY_SECOND) {
                Some(cut) => cut.to_string(),
                None => text,
            };
            out.push(ViewerUpdate::Note { text });
            None
        }
        Update::Handoff(Ok(_)) => {
            out.push(ViewerUpdate::HandoffTaken);
            None
        }
        Update::Handoff(Err(detail)) => {
            out.push(ViewerUpdate::HandoffRefused { detail });
            None
        }
    };
    out.extend(crossed.map(|detail| ViewerUpdate::Failed { detail }));
}

/// The events that decode, in order, and what the others were, if any.
fn decode(events: &[Value]) -> (Vec<LogEvent>, Option<String>) {
    let mut decoded = Vec::with_capacity(events.len());
    let mut failed = Vec::new();
    for v in events {
        match LogEvent::deserialize(v) {
            Ok(e) => decoded.push(e),
            Err(e) => failed.push(format!("{} ({e})", v["id"].as_str().unwrap_or("with no id"))),
        }
    }
    let detail = (!failed.is_empty()).then(|| format!("{} event(s) this build cannot show were left out — the app may need updating: {}", failed.len(), failed.join("; ")));
    (decoded, detail)
}

/// What `ViewerCore::chunks_read` takes: the events read and where the
/// reading stopped, or why it failed.
type Read = Result<(Vec<Value>, Attached), String>;

/// The chunks one `ReadChunks` read, or why the read failed: opaque, handed
/// back to the `Viewer` that asked, once.
#[derive(uniffi::Object)]
pub struct ChunksRead {
    /// The viewer that asked, and which of its `ReadChunks` this answers:
    /// any other viewer's, or one answering an earlier one, is dropped.
    viewer: u64,
    asked: u64,
    read: Mutex<Option<Read>>,
}

/// Each `Viewer`'s own number, for its `ChunksRead`s.
static VIEWERS: AtomicU64 = AtomicU64::new(1);

/// A synced session followed live through the relay. Swift calls it from
/// one serial queue; the mutex is for safety, not parallelism.
#[derive(uniffi::Object)]
pub struct Viewer {
    id: u64,
    inner: Mutex<Inner>,
}

struct Inner {
    core: ViewerCore,
    opened: Arc<OpenedSession>,
    /// `ViewerCore::new`'s effects, until `start`.
    first: Option<Vec<core::Effect>>,
    /// The `extra` of the join asked for and not yet dialed.
    pending: Option<Value>,
    /// The join dialed and not yet answered `joined`, and by when the
    /// relay must send its next message.
    joining: Option<(Joining, u64)>,
    /// The connection Swift holds for this viewer, if any: the one dialed
    /// last, until it is dropped or closed. Inputs from any other are
    /// ignored.
    connection: Option<u64>,
    dialed: u64,
    /// The `ReadChunks` out, by number, and the head it reads to.
    asked: u64,
    want: Option<Head>,
    /// When to join again, and what wakes it.
    sup: Arc<ConnectionSupervisor>,
    /// A join is pending whose `NeedTicket` waits for the foreground.
    held: bool,
    /// When the pending join's `NeedTicket` said to fetch its ticket.
    ticket_at: Option<u64>,
    /// The next `Join` the core asks for replaces a connection now, on the
    /// supervisor's word, rather than backing off.
    replacing: bool,
}

/// What a viewer whose lock was poisoned says, every time. A panic aborts
/// the app in the release profile, and Swift's generated code stops at the
/// first one anyway, so this is a backstop rather than a path: nothing a
/// caller passes in panics.
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
    /// relay `Account` chose for it. Nothing happens until `start`. A relay
    /// URL with no origin a relay signs (`ws://` or `wss://` and a host) is
    /// `InvalidInput`. Waits for a `catch_up` of the session's that is
    /// running, so call it off the main thread; never touches the network
    /// itself. The viewer reads on from its own copy: what it reads is not
    /// `session`'s `head` or `events`.
    #[uniffi::constructor]
    pub fn open(session: Arc<OpenedSession>, now_ms: u64) -> Result<Arc<Viewer>, KrowkError> {
        let seed = session.id().bytes().fold(now_ms, |h, b| h.rotate_left(5) ^ u64::from(b));
        Viewer::open_supervised(session, ConnectionSupervisor::new(now_ms, seed), now_ms)
    }

    /// `open`, its joins timed by `supervisor` (ticket 86): the one the app
    /// opened the session under, so a backoff begun while opening goes on.
    #[uniffi::constructor]
    pub fn open_supervised(session: Arc<OpenedSession>, supervisor: Arc<ConnectionSupervisor>, now_ms: u64) -> Result<Arc<Viewer>, KrowkError> {
        let relay = session.relay().trim_end_matches('/');
        if e2e::canonical_origin(relay).is_none() {
            return Err(KrowkError::InvalidInput { detail: format!("{relay} is not a relay address — it is ws:// or wss:// and a host") });
        }
        let at_rest = session.snapshot();
        let (core, first) = ViewerCore::new(session.keys().clone(), &session.id(), session.device(), session.relay(), at_rest, now_ms);
        let inner = Inner { core, opened: session, first: Some(first), pending: None, joining: None, connection: None, dialed: 0, asked: 0, want: None, sup: supervisor, held: false, ticket_at: None, replacing: false };
        Ok(Arc::new(Viewer { id: VIEWERS.fetch_add(1, Ordering::Relaxed), inner: Mutex::new(inner) }))
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
            // Asked for before the app went to the background: not dialed
            // there, but held with the join for the foreground.
            if i.pending.is_some() && i.sup.in_background() {
                i.held = true;
                i.ticket_at = None;
                return Vec::new();
            }
            let Some(extra) = i.pending.take() else { return Vec::new() };
            let o = &i.opened;
            let id = o.id();
            match Joining::new(o.relay(), &id, o.env(), o.device(), RELAY_ROLE_VIEWER, extra) {
                Ok(j) => {
                    let d = join::dial(o.relay(), &id, o.env(), &ticket);
                    i.joining = Some((j, now_ms.saturating_add(JOIN_WAIT_MS)));
                    i.dialed += 1;
                    i.connection = Some(i.dialed);
                    vec![Effect::Dial { url: d.url, ticket: d.ticket, connection: i.dialed }]
                }
                Err(e) => {
                    let out = i.core.join_failed(e, now_ms);
                    i.effects(out, now_ms)
                }
            }
        })
    }

    /// The ticket a `NeedTicket` asked for could not be had, and why: said
    /// to the person, and asked for again later. Nothing if no join is
    /// waiting for one.
    pub fn ticket_failed(&self, detail: String, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if i.pending.take().is_none() {
                return Vec::new();
            }
            let out = i.core.join_failed(detail, now_ms);
            i.effects(out, now_ms)
        })
    }

    /// A message `connection` received. Ignored unless it is the current
    /// connection.
    pub fn received(&self, connection: u64, frame: WsFrame, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if i.connection != Some(connection) {
                return Vec::new();
            }
            i.sup.with(|s| s.rules.heard(now_ms));
            if let Some((j, by)) = i.joining.as_mut() {
                *by = now_ms.saturating_add(JOIN_WAIT_MS);
                let WsFrame::Binary { bytes } = frame else { return Vec::new() };
                let out = match j.message(i.opened.signing(), &bytes) {
                    Ok(Step::Wait) => return Vec::new(),
                    Ok(Step::Send(bytes)) => return vec![Effect::Send { bytes }],
                    Ok(Step::Joined(v)) => {
                        i.joining = None;
                        i.sup.with(|s| s.rules.joined(now_ms));
                        i.core.joined(&v, now_ms)
                    }
                    Err(e) => {
                        i.joining = None;
                        i.core.join_failed(e, now_ms)
                    }
                };
                return i.effects(out, now_ms);
            }
            match frame {
                WsFrame::Text { .. } => {
                    i.core.alive(now_ms);
                    Vec::new()
                }
                WsFrame::Binary { bytes } => {
                    let out = i.core.received(&bytes, now_ms);
                    i.effects(out, now_ms)
                }
            }
        })
    }

    /// `connection` ended, or failed to open. Ignored unless it is the
    /// current connection.
    pub fn closed(&self, connection: u64, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if i.connection != Some(connection) {
                return Vec::new();
            }
            i.connection = None;
            let out = match i.joining.take() {
                Some(_) => i.core.join_failed("the connection to the relay ended before it answered the join".into(), now_ms),
                None => i.core.closed(now_ms),
            };
            i.effects(out, now_ms)
        })
    }

    /// The time passing: call it when `next_wake_ms` says. A join the relay
    /// has left unanswered too long fails here, and is tried again.
    pub fn tick(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let mut out = Vec::new();
            if i.joining.as_ref().is_some_and(|(_, by)| now_ms >= *by) {
                i.joining = None;
                let failed = i.core.join_failed(format!("the relay did not answer the join within {} seconds", JOIN_WAIT_MS / 1000), now_ms);
                out = i.effects(failed, now_ms);
            }
            // A wake's probe unanswered past its 5 s: replaced.
            if i.link(now_ms) == LinkState::Up && i.sup.with(|s| s.rules.probe_due(now_ms)) {
                out.extend(i.replace(now_ms));
            }
            let ticked = i.core.tick(now_ms);
            out.extend(i.effects(ticked, now_ms));
            out
        })
    }

    /// The scene became active, a network path became usable, or the
    /// person pulled to refresh: with no connection, a ticket now; one
    /// silent past `DEAD`, replaced now; else a `Ping`, and replaced at a
    /// later `tick` if nothing comes back within 5 s (`next_wake_ms` says
    /// when). Nothing in the background.
    pub fn wake(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let link = i.link(now_ms);
            let w = i.sup.wake(now_ms, link);
            i.act(w, now_ms)
        })
    }

    /// The scene went to the background: the connection is kept, but one
    /// lost from here is not joined again until `foreground`. A join
    /// waiting for its ticket (on the backoff, or being fetched) is held
    /// too: Swift drops the ticket it asked for, and `foreground` asks
    /// afresh, so a fetch iOS suspended never holds the return up.
    pub fn background(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            i.sup.background(now_ms);
            if i.pending.is_some() && i.joining.is_none() {
                i.held = true;
                i.ticket_at = None;
            }
            Vec::new()
        })
    }

    /// The scene is coming forward: after 10 s or more away the
    /// connection is replaced at once, one lost meanwhile joined now, and
    /// otherwise as `wake`.
    pub fn foreground(&self, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            let link = i.link(now_ms);
            let w = i.sup.foreground(now_ms, link);
            i.act(w, now_ms)
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
        let i = self.inner.lock().ok()?;
        let core = i.core.next_wake()?;
        let at = i.joining.as_ref().map_or(core, |(_, by)| core.min(*by));
        Some(i.sup.with(|s| s.rules.next_wake()).map_or(at, |p| at.min(p)))
    }

    /// Reads the chunks the host wrote since the viewer's last read, up to
    /// the head its welcome named, for `chunks_read`. A failed read is in
    /// what it returns too: hand it on all the same. Blocking: a round trip
    /// or more to the registry, with the viewer's lock not held; call it
    /// off the main thread, after a `ReadChunks`.
    pub fn read_chunks(&self) -> Arc<ChunksRead> {
        let copied = self.inner.lock().map(|i| {
            // Where the reading stopped, without the events read so far:
            // `store::catch_up` only adds to them, and the core reads its
            // at-rest events only as it starts.
            let a = i.core.attached();
            let from = Attached { index: a.index.clone(), events: Vec::new(), head: a.head, next: a.next, previous: a.previous, fence: a.fence, epoch: a.epoch, heads: a.heads.clone() };
            (i.asked, i.want, from, Arc::clone(&i.opened))
        });
        let (asked, read) = match copied {
            Ok((asked, want, mut a, o)) => (asked, store::catch_up(o.client(), o.keys(), &o.id(), &mut a, want).map(|fresh| (fresh, a))),
            Err(_) => (0, Err("the viewer failed inside".into())),
        };
        Arc::new(ChunksRead { viewer: self.id, asked, read: Mutex::new(Some(read)) })
    }

    /// What `read_chunks` returned. One another viewer read, one answering
    /// an earlier `ReadChunks` than the last, or one handed in twice, is
    /// dropped.
    pub fn chunks_read(&self, read: Arc<ChunksRead>, now_ms: u64) -> Vec<Effect> {
        self.with(|i| {
            if read.viewer != self.id || read.asked != i.asked {
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
    /// Where the connection stands, for the supervisor.
    fn link(&self, now_ms: u64) -> LinkState {
        if self.joining.is_some() || (self.connection.is_none() && self.pending.is_some() && !self.held && self.ticket_at.is_some_and(|at| at <= now_ms)) {
            LinkState::Joining
        } else if self.connection.is_some() {
            LinkState::Up
        } else {
            LinkState::Down
        }
    }

    /// What a wake or a foreground asked for, carried out.
    fn act(&mut self, w: WakeAction, now_ms: u64) -> Vec<Effect> {
        match w {
            WakeAction::Nothing => Vec::new(),
            WakeAction::Probe => vec![Effect::Ping],
            WakeAction::Replace => self.replace(now_ms),
            // The join waiting on its backoff, or held in the background:
            // its ticket now.
            WakeAction::Connect if self.pending.is_some() => {
                self.held = false;
                self.ticket_at = Some(now_ms);
                vec![Effect::NeedTicket { at_ms: now_ms }]
            }
            WakeAction::Connect => Vec::new(),
        }
    }

    /// The current connection dropped and joined again at once, from the
    /// cursor, as if it had closed: no backoff. Mid-read, the core acts on
    /// the close once the chunks are read.
    ///
    /// Once: a replace already asked waits for its join, and the probe
    /// that asked it is done with, so a tick never asks again (a read out
    /// would otherwise queue a close every tick).
    fn replace(&mut self, now_ms: u64) -> Vec<Effect> {
        if self.connection.is_none() || self.joining.is_some() || self.replacing {
            return Vec::new();
        }
        self.sup.with(|s| s.rules.replaced(now_ms));
        self.replacing = true;
        let out = self.core.closed(now_ms);
        self.effects(out, now_ms)
    }

    /// The core's effects as Swift carries them out. No `_` arm: a new
    /// `core::Effect` fails this build until it is handled.
    fn effects(&mut self, from: Vec<core::Effect>, now_ms: u64) -> Vec<Effect> {
        let mut out = Vec::with_capacity(from.len());
        for e in from {
            match e {
                core::Effect::Send(bytes) => out.push(Effect::Send { bytes }),
                core::Effect::Ping => out.push(Effect::Ping),
                core::Effect::Show(shown) => {
                    let mut crossed = Vec::with_capacity(shown.len());
                    for u in shown {
                        updates(u, &mut crossed);
                    }
                    out.push(Effect::Show { updates: crossed });
                }
                core::Effect::ReadChunks { want } => {
                    self.asked += 1;
                    self.want = want;
                    out.push(Effect::ReadChunks);
                }
                // When is the supervisor's: a join the core wants now (the
                // first, a dead link's) or one replacing on its word goes
                // now; one after a loss (the core's second later) waits on
                // the backoff; in the background, any waits for the
                // foreground.
                core::Effect::Join { extra, at_ms } => {
                    if self.connection.take().is_some() {
                        out.push(Effect::Close);
                    }
                    self.joining = None;
                    self.pending = Some(extra);
                    let replacing = std::mem::take(&mut self.replacing);
                    let at = self.sup.with(|s| {
                        if !s.rules.in_background() && (replacing || at_ms <= now_ms) {
                            s.rules.replaced(now_ms);
                            Some(at_ms.min(now_ms))
                        } else {
                            s.lost(now_ms)
                        }
                    });
                    self.held = at.is_none();
                    self.ticket_at = at;
                    out.extend(at.map(|at_ms| Effect::NeedTicket { at_ms }));
                }
                // Relay only on the phone: a race for a direct path is lost
                // as it is asked, and the core asks again later, backing off.
                core::Effect::Probe => self.core.probe_lost(now_ms),
                // Only a move to a direct path leaves a connection behind.
                core::Effect::Leave => {}
                core::Effect::End => {
                    self.pending = None;
                    self.joining = None;
                    self.connection = None;
                    self.held = false;
                    out.push(Effect::End);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // The wrapper's own pieces; the protocol is tests/viewer.rs's.
    use super::*;

    fn crossed(u: Update) -> Vec<ViewerUpdate> {
        let mut out = Vec::new();
        updates(u, &mut out);
        out
    }

    #[test]
    fn r_client_1_events_that_do_not_decode_are_left_out_and_said() {
        let known = serde_json::json!({"id": "01a0ec7b-2222-7000-8000-000000000001", "sessionId": "s", "timeMs": 1, "type": "run.opened", "turnId": "t", "run": "r"});
        let newer = serde_json::json!({"id": "x", "type": "turn.teleported"});
        match crossed(Update::CaughtUp(vec![newer.clone(), known])).as_slice() {
            [ViewerUpdate::CaughtUp { events }, ViewerUpdate::Failed { detail }] => {
                assert_eq!(events.len(), 1);
                assert!(detail.starts_with("1 event(s)") && detail.contains("x ("), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(crossed(Update::CaughtUp(vec![newer.clone()])).as_slice(), [ViewerUpdate::Failed { .. }]));
        assert!(matches!(crossed(Update::Attached { events: vec![newer], head: None }).as_slice(), [ViewerUpdate::Attached { events, head: None }, ViewerUpdate::Failed { .. }] if events.is_empty()));
        assert!(matches!(crossed(Update::Handoff(Err("no".into()))).as_slice(), [ViewerUpdate::HandoffRefused { detail }] if detail == "no"));
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

    #[test]
    fn r_client_1_a_dial_never_logs_its_ticket() {
        let d = format!("{:?}", Effect::Dial { url: "wss://r/v1/relay/s".into(), ticket: "secret".into(), connection: 1 });
        assert!(!d.contains("secret") && d.contains("wss://r"), "{d}");
    }
}
