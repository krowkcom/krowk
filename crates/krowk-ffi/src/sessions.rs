//! The person's synced sessions, as the phone lists and opens them
//! (R-IOS-3, R-CLIENT-1): `krowk_client::sync::store`'s listing and at-rest
//! reader, behind [`Account`]. Listing verifies each session record against
//! the device list this phone holds and unseals its index; opening unwraps
//! the session key and reads the log from its latest checkpoint
//! (R-PERF-6), never from chunk 0. Every call here is a blocking round trip
//! (or several) to the registry: Swift calls them off the main thread.
//!
//! The session's keys, the device's signing key and the read position stay
//! inside [`OpenedSession`]; nothing secret crosses to Swift. The live
//! viewer reads them crate-internally.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use krowk_api::Client;
use krowk_client::e2e::{DeviceId, SessionKeys, SigningKey};
use krowk_client::protocol::LogEvent;
use krowk_client::session_record::Signer;
use krowk_client::sync::store::{self, Attached, Head, Listed, ReadError, ReadPlace};
use serde::Deserialize;
use serde_json::Value;

use crate::KrowkError;
use crate::account::{Account, internal};

/// The relay a phone uses with krowk's own registry.
const HOSTED_RELAY: &str = "wss://relay.krowk.com";

/// Where `krowk relay serve` listens by default: the relay with any other
/// registry, as the terminal picks it, unless the app names one.
const LOCAL_RELAY: &str = "ws://127.0.0.1:7790";

/// Where a session's log ends: its last chunk's index and the SHA-256 of
/// that chunk sealed (32 bytes). What the phone keeps of a session it has
/// read and hands back to `open_session` as `known`, so a registry serving
/// it a shorter log is caught.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HeadRecord {
    pub index: u64,
    pub digest: Vec<u8>,
}

impl From<Head> for HeadRecord {
    fn from(h: Head) -> HeadRecord {
        HeadRecord { index: h.index, digest: h.digest.to_vec() }
    }
}

impl TryFrom<HeadRecord> for Head {
    type Error = KrowkError;

    fn try_from(h: HeadRecord) -> Result<Head, KrowkError> {
        let digest = <[u8; 32]>::try_from(h.digest.as_slice())
            .map_err(|_| KrowkError::InvalidInput { detail: format!("a head's digest is 32 bytes, not {}", h.digest.len()) })?;
        Ok(Head { index: h.index, digest })
    }
}

/// Where the phone's reading of a session stopped (`OpenedSession::place`):
/// the next chunk's index, the digest it must bind (32 bytes), the lease
/// fence and key epoch it may not go below, and the head read. Kept with
/// the events read, it is what `resume_session` reads on from.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReadPlaceRecord {
    pub next: u64,
    pub previous: Vec<u8>,
    pub fence: u64,
    pub epoch: u32,
    pub head: HeadRecord,
}

impl From<ReadPlace> for ReadPlaceRecord {
    fn from(p: ReadPlace) -> ReadPlaceRecord {
        ReadPlaceRecord { next: p.next, previous: p.previous.to_vec(), fence: p.fence, epoch: p.epoch, head: p.head.into() }
    }
}

impl TryFrom<ReadPlaceRecord> for ReadPlace {
    type Error = KrowkError;

    fn try_from(p: ReadPlaceRecord) -> Result<ReadPlace, KrowkError> {
        let previous = <[u8; 32]>::try_from(p.previous.as_slice())
            .map_err(|_| KrowkError::InvalidInput { detail: format!("a read place's previous digest is 32 bytes, not {}", p.previous.len()) })?;
        let place = ReadPlace { next: p.next, previous, fence: p.fence, epoch: p.epoch, head: p.head.try_into()? };
        place.check().map_err(|detail| KrowkError::InvalidInput { detail })?;
        Ok(place)
    }
}

/// The sessions this phone can open, newest first, and how many more it
/// cannot open yet: sealed under a user key it does not hold, or recorded
/// by a device its device list never held.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    pub unreadable: u32,
}

/// One session as the list screen shows it, from its sealed index.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionSummary {
    pub id: String,
    /// Empty when the session has none yet.
    pub title: String,
    pub cwd: String,
    /// When its host last wrote it, in milliseconds since the epoch.
    pub updated_ms: u64,
    /// Where its log ended when the index was last written.
    pub head: Option<HeadRecord>,
    /// The device id holding the session's lease — hosting it now — when
    /// one does.
    pub hosted_by: Option<String>,
    /// The device id that last took the session up, from the index.
    pub host_device: Option<String>,
}

impl From<Listed> for SessionSummary {
    fn from(l: Listed) -> SessionSummary {
        SessionSummary {
            id: l.id,
            title: l.index.title,
            cwd: l.index.cwd,
            updated_ms: l.index.updated_ms,
            head: l.index.head.map(Into::into),
            hosted_by: l.holder,
            host_device: l.index.host.map(|h| h.device),
        }
    }
}

#[uniffi::export]
impl Account {
    /// The person's synced sessions this phone can open, newest first, each
    /// record checked against the device list it holds and its index
    /// unsealed; the rest are counted in `unreadable`, not an error.
    /// Refused with `not_paired` until the phone holds the user keys and
    /// the device list. A registry call that fails fails the listing, as
    /// `Internal` with the registry's words. Blocking: one call per page of
    /// the listing and one per session; call it off the main thread.
    pub fn list_sessions(&self) -> Result<SessionList, KrowkError> {
        let (keys, chain) = self.paired()?;
        let (listed, unreadable) = store::list(&self.client, &keys, &chain).map_err(internal)?;
        Ok(SessionList { sessions: listed.into_iter().map(Into::into).collect(), unreadable: u32::try_from(unreadable).unwrap_or(u32::MAX) })
    }

    /// Opens session `id` and reads its log from the latest checkpoint to
    /// the end the registry serves. A log that ends before the head its
    /// sealed index names, or before `known` — the head this phone saw of
    /// it before (`OpenedSession::head`, kept) — or holds another chunk at
    /// that head's place, is refused as `session_rolled_back`. One whose
    /// record or index this phone cannot open is refused as
    /// `session_unreadable`; `not_paired` as for `list_sessions`; a chunk
    /// that fails to read or open is `Internal`. The session keeps
    /// `relay_url(relay_override)` as its relay. Blocking: a few round
    /// trips, chunks fetched eight at a time; call it off the main thread.
    pub fn open_session(&self, id: String, known: Option<HeadRecord>, relay_override: Option<String>) -> Result<Arc<OpenedSession>, KrowkError> {
        let known = known.map(Head::try_from).transpose()?;
        let (keys, chain) = self.paired()?;
        let full = self.client.show_sync_session(&id)?;
        let unreadable = |detail: String| KrowkError::Refused { code: "session_unreadable".into(), detail };
        let key = store::open_session_key(&full, &id, &keys, &chain, Signer::EverHeld).map_err(unreadable)?;
        let index = store::open_index(&key, &id, &full.sealed_index).map_err(unreadable)?;
        // Held to the index's head and to `known` here rather than inside
        // `attach`, so a rollback is told apart from a read that failed: the
        // same two checks `attach` makes, in the same order.
        let sealed_head = index.head;
        let mut attached = store::attach(&self.client, &key, &id, store::Index { head: None, ..index }, None).map_err(internal)?;
        Head::at_least(&attached.heads, sealed_head).map_err(rolled_back)?;
        Head::at_least(&attached.heads, known).map_err(rolled_back)?;
        attached.index.head = sealed_head;
        self.opened(id, key, attached, relay_override)
    }

    /// Opens session `id` from `place`, where this phone's reading of it
    /// stopped before (`OpenedSession::place`, kept with the events it
    /// read), and reads only the chunks after it: `events` are only those
    /// after `place`. Held to the same chain, then to the sealed index's
    /// head and to `known`. An index older than `place` (the registry
    /// rolled back past what this phone read), a chunk after it that does
    /// not follow it (another log), and the checks `open_session` makes,
    /// are refused as `session_rolled_back`. A `place` no reading stopped
    /// at, or whose digests are not 32 bytes, is `InvalidInput`. Otherwise
    /// as `open_session`. Blocking; call it off the main thread.
    pub fn resume_session(&self, id: String, place: ReadPlaceRecord, known: Option<HeadRecord>, relay_override: Option<String>) -> Result<Arc<OpenedSession>, KrowkError> {
        let place = ReadPlace::try_from(place)?;
        let known = known.map(Head::try_from).transpose()?;
        let (keys, chain) = self.paired()?;
        let full = self.client.show_sync_session(&id)?;
        let unreadable = |detail: String| KrowkError::Refused { code: "session_unreadable".into(), detail };
        let key = store::open_session_key(&full, &id, &keys, &chain, Signer::EverHeld).map_err(unreadable)?;
        let index = store::open_index(&key, &id, &full.sealed_index).map_err(unreadable)?;
        let attached = store::resume(&self.client, &key, &id, index, place, known).map_err(read_failed)?;
        self.opened(id, key, attached, relay_override)
    }

    /// The relay the phone joins sessions through: `relay_override` when it
    /// names one, else krowk's hosted relay for krowk's own registry, else
    /// the address `krowk relay serve` listens on by default — as the
    /// terminal picks it. That last is the phone itself, so the app names
    /// the relay for any registry but krowk's (the fake fleet's, in tests).
    pub fn relay_url(&self, relay_override: Option<String>) -> String {
        match relay_override.as_deref().map(str::trim) {
            Some(r) if !r.is_empty() => r.to_owned(),
            _ if self.api.trim_end_matches('/') == krowk_api::DEFAULT_BASE_URL => HOSTED_RELAY.to_owned(),
            _ => LOCAL_RELAY.to_owned(),
        }
    }

    /// The relay env the registry's tickets name: `production` only for
    /// krowk's own registry in a release build of the app, else
    /// `development`.
    pub fn relay_env(&self) -> String {
        krowk_api::relay_env(&self.api, &|_| String::new()).to_owned()
    }
}

impl Account {
    fn opened(&self, id: String, keys: SessionKeys, attached: Attached, relay_override: Option<String>) -> Result<Arc<OpenedSession>, KrowkError> {
        let signing = self.keystore.signing_key().map_err(internal)?;
        Ok(Arc::new(OpenedSession {
            id,
            keys,
            title: attached.index.title.clone(),
            reading: Mutex::new(()),
            attached: Mutex::new(attached),
            client: Arc::clone(&self.client),
            device: self.device,
            signing,
            relay: self.relay_url(relay_override),
            env: self.relay_env(),
        }))
    }

    /// The user keys and the device list this phone holds; `not_paired`
    /// until it holds both.
    fn paired(&self) -> Result<(krowk_client::user_key::UserKeys, krowk_client::device_chain::Chain), KrowkError> {
        let keys = self.keystore.user_keys().map_err(internal)?;
        let chain = self.keystore.device_list().map_err(internal)?;
        match (keys, chain) {
            (Some(k), Some(c)) => Ok((k, c)),
            _ => Err(KrowkError::Refused { code: "not_paired".into(), detail: "this phone is not paired yet — add it from a computer with `krowk devices add`".into() }),
        }
    }
}

/// A synced session, opened: its keys and where the reading stopped, held
/// here and never handed to Swift. Read again with `catch_up`; joined live
/// through the relay with `relay_ticket`.
#[derive(uniffi::Object)]
pub struct OpenedSession {
    id: String,
    keys: SessionKeys,
    /// From the sealed index, which an open reads once.
    title: String,
    /// Held for the whole of a `catch_up`, so two take turns; `attached`
    /// only while one is read or committed, so the getters never wait on
    /// the network.
    reading: Mutex<()>,
    attached: Mutex<Attached>,
    client: Arc<Client>,
    device: DeviceId,
    signing: SigningKey,
    relay: String,
    env: String,
}

#[uniffi::export]
impl OpenedSession {
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// From the sealed index as the session was opened; empty when it has
    /// none. Never blocks.
    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// Where the log read so far ends: what to keep and pass to
    /// `open_session` as `known` next time. None for a log with no chunk.
    /// Never waits on the network, a `catch_up` running or not.
    pub fn head(&self) -> Option<HeadRecord> {
        self.attached().head.map(Into::into)
    }

    /// Where the reading stopped: what to keep with the events read and
    /// pass to `resume_session` next time, so only what came after is
    /// read. None for a log with no chunk. Never waits on the network.
    pub fn place(&self) -> Option<ReadPlaceRecord> {
        self.attached().place().map(Into::into)
    }

    /// Every event read so far, from the checkpoint on (from the place, for
    /// a session `resume_session` opened), in order: each
    /// decoded again on every call, so the app keeps what it shows. One
    /// that this build cannot decode — a newer host's — is a `Decode`
    /// error, not dropped. Never waits on the network.
    pub fn events(&self) -> Result<Vec<LogEvent>, KrowkError> {
        decode(&self.attached().events, 0)
    }

    /// Reads what the host wrote since the last read and returns only those
    /// events. A read that fails, or new events that do not decode
    /// (`Decode`), leave the session where it was, so the next call reads
    /// them again rather than skipping them. Blocking: a round trip, more
    /// when there is more; call it off the main thread. Two at once take
    /// turns; the other methods do not wait for it.
    pub fn catch_up(&self) -> Result<Vec<LogEvent>, KrowkError> {
        let _turn = self.reading.lock().unwrap_or_else(PoisonError::into_inner);
        // Where the reading stopped, without the events read so far: the
        // read runs on this copy, and only a read that decodes is kept.
        // `heads` is lent, not copied: only a `catch_up` touches it, and this
        // one holds the turn. It goes back however the read ends, cut to
        // what it held when nothing read is kept.
        let (mut read, before, lent) = {
            let mut a = self.attached();
            let heads = std::mem::take(&mut a.heads);
            let lent = heads.len();
            (Attached { index: a.index.clone(), events: Vec::new(), head: a.head, next: a.next, previous: a.previous, fence: a.fence, epoch: a.epoch, heads }, a.events.len(), lent)
        };
        let decoded = store::catch_up(&self.client, &self.keys, &self.id, &mut read, None)
            .map_err(|e| internal(e.into()))
            .and_then(|fresh| decode(&fresh, before).map(|d| (fresh, d)));
        let mut a = self.attached();
        let (fresh, decoded) = match decoded {
            Ok(got) => got,
            Err(e) => {
                // `store::catch_up` adds the chunks it opened to `heads` even
                // when it fails; they are read again next time.
                read.heads.truncate(lent);
                a.heads = read.heads;
                return Err(e);
            }
        };
        a.events.extend(fresh);
        a.head = read.head;
        a.next = read.next;
        a.previous = read.previous;
        a.fence = read.fence;
        a.epoch = read.epoch;
        a.heads = read.heads;
        Ok(decoded)
    }

    /// A relay ticket for this device to join the session as a viewer, from
    /// the registry. Blocking: one round trip.
    pub fn relay_ticket(&self) -> Result<String, KrowkError> {
        Ok(self.client.relay_ticket(&self.id, &self.device.to_string(), &self.env)?.relay_ticket)
    }
}

// What the live viewer (`crate::viewer`) reads to join the relay and open
// what arrives, and nothing outside the crate can.
impl OpenedSession {
    /// The session as read so far, copied whole: taken under the `reading`
    /// turn, so a `catch_up` running is waited for rather than copied with
    /// its `heads` lent out. Blocks while one runs.
    pub(crate) fn snapshot(&self) -> Attached {
        let _turn = self.reading.lock().unwrap_or_else(PoisonError::into_inner);
        self.attached().clone()
    }

    pub(crate) fn keys(&self) -> &SessionKeys {
        &self.keys
    }

    pub(crate) fn client(&self) -> &Arc<Client> {
        &self.client
    }

    pub(crate) fn device(&self) -> DeviceId {
        self.device
    }

    pub(crate) fn signing(&self) -> &SigningKey {
        &self.signing
    }

    pub(crate) fn relay(&self) -> &str {
        &self.relay
    }

    pub(crate) fn env(&self) -> &str {
        &self.env
    }

    /// The session as read so far; held while the caller reads it, so a
    /// `catch_up` waits to commit what it read. Only to read: a `catch_up`
    /// commits over the fields it read from, so whatever changes `Attached`
    /// takes the `reading` turn first, as `catch_up` does. `heads` is lent
    /// out, and empty, while a `catch_up` runs: read it under the turn.
    pub(crate) fn attached(&self) -> MutexGuard<'_, Attached> {
        self.attached.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn rolled_back(detail: String) -> KrowkError {
    KrowkError::Refused { code: "session_rolled_back".into(), detail }
}

/// A read at rest that failed: `session_rolled_back` when the log is not
/// the one this phone read or was told of, else `Internal`.
fn read_failed(e: ReadError) -> KrowkError {
    if e.rolled_back { rolled_back(e.detail) } else { internal(e.detail) }
}

/// The log's events, the first of them the `from`th read since the
/// checkpoint, each decoded; the first that does not decode fails the lot,
/// naming its place.
fn decode(events: &[Value], from: usize) -> Result<Vec<LogEvent>, KrowkError> {
    events
        .iter()
        .enumerate()
        .map(|(i, v)| {
            LogEvent::deserialize(v).map_err(|e| KrowkError::Decode {
                detail: format!("event {} read since the session's checkpoint is not a log event this build understands — the app may need updating: {e}", from + i),
            })
        })
        .collect()
}
