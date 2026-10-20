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
use krowk_client::sync::store::{self, Attached, Head, Listed};
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
        let rolled_back = |detail: String| KrowkError::Refused { code: "session_rolled_back".into(), detail };
        Head::at_least(&attached.heads, sealed_head).map_err(rolled_back)?;
        Head::at_least(&attached.heads, known).map_err(rolled_back)?;
        attached.index.head = sealed_head;
        let signing = self.keystore.signing_key().map_err(internal)?;
        Ok(Arc::new(OpenedSession {
            id,
            keys: key,
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

    /// Every event read so far, from the checkpoint on, in order: each
    /// decoded again on every call, so the app keeps what it shows. One
    /// that this build cannot decode — a newer host's — is a `Decode`
    /// error, not dropped. Never waits on the network.
    pub fn events(&self) -> Result<Vec<LogEvent>, KrowkError> {
        decode(&self.attached().events)
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
        let mut read = {
            let a = self.attached();
            Attached { index: a.index.clone(), events: Vec::new(), head: a.head, next: a.next, previous: a.previous, fence: a.fence, epoch: a.epoch, heads: a.heads.clone() }
        };
        let fresh = store::catch_up(&self.client, &self.keys, &self.id, &mut read, None).map_err(internal)?;
        let decoded = decode(&fresh)?;
        let mut a = self.attached();
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

// What the live viewer reads to join the relay and open what arrives, and
// nothing outside the crate can.
#[allow(dead_code)] // The live viewer (ticket 80) is the first to read these.
impl OpenedSession {
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
    /// `catch_up` waits to commit what it read.
    pub(crate) fn attached(&self) -> MutexGuard<'_, Attached> {
        self.attached.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The log's events, each decoded; the first that does not decode fails
/// the lot, naming its place.
fn decode(events: &[Value]) -> Result<Vec<LogEvent>, KrowkError> {
    events
        .iter()
        .enumerate()
        .map(|(i, v)| LogEvent::deserialize(v).map_err(|e| KrowkError::Decode { detail: format!("event {i} of the session's log is not a log event this build understands: {e}") }))
        .collect()
}
