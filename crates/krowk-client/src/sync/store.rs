//! A synced session at rest (engineering/harness.md → Sync): the shapes of
//! its sealed index and of the chunks of its log, checkpoints among them,
//! and reading them back on a device listing them (`list`) or attaching
//! (`attach`, `catch_up`, `resume` from a kept place, the key and index
//! openers), so no client reimplements the crypto (R-CLIENT-1). Writing
//! them is the lease holder's, in the harness
//! (`krowk_harness::sync::store::Writer`). Blocking: each read is a round
//! trip through `krowk_api::Client`, so a phone calls these off its main
//! thread.
//!
//! **A chunk** is JSON, sealed with `e2e::ChunkSealer`: `{"type": "events",
//! "events": [LogEvent…]}`, the events logged since the chunk before, cut at
//! each turn's end; or `{"type": "checkpoint", …}` (`Checkpoint`), which
//! holds everything a device attaching needs up to its place in the log.
//!
//! **The sealed index** (`Index`) names the session for a listing and says
//! where the log stands: the head (the last chunk's index and digest) and
//! the latest checkpoint's place in the chain. A viewer starts reading at
//! the checkpoint and must reach the head. The index is one message the
//! holder replaces, so a hostile registry can serve an older one together
//! with the log as it was then; what detects that is the host's live head
//! in its welcome, and a device's own high-water mark (`Head::at_least`).

use crate::device_chain::Chain;
use crate::e2e::{self, ChunkReader, SessionKeys};
use crate::session_record;
use crate::user_key::UserKeys;
use krowk_api::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a log ends: the last chunk's index and the SHA-256 of it sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub index: u64,
    #[serde(with = "hex32")]
    pub digest: [u8; 32],
}

impl Head {
    /// Refuses a log that ends before `known`, a head this device has seen
    /// vouched for (the host's welcome, or what it read before): a registry
    /// serving a prefix, or an index older than the log. `read` is every
    /// chunk this reading opened, in order: a log that reaches past `known`
    /// must hold `known`'s own chunk at its index, or it is another log.
    pub fn at_least(read: &[Head], known: Option<Head>) -> Result<(), String> {
        let Some(k) = known else { return Ok(()) };
        let last = read.last().copied();
        match last {
            Some(r) if r.index >= k.index => match read.iter().find(|h| h.index == k.index) {
                Some(h) if h.digest != k.digest => Err(format!("chunk {} is not the one the session's host sealed: the registry served another log", k.index)),
                // Before what this reading opened (under the checkpoint it
                // started from): the chain from there binds it.
                _ => Ok(()),
            },
            _ => Err(format!("the registry served the log only to chunk {}, where the session's host has sealed to chunk {}: a prefix, refused", last.map_or(-1, |r| r.index as i64), k.index)),
        }
    }
}

/// Where the latest checkpoint sits: its index, the digest of the chunk
/// before it, and the fence that chunk was written under, which is what a
/// reader starting there needs (`ChunkReader::resume`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub index: u64,
    #[serde(with = "hex32")]
    pub previous: [u8; 32],
    pub fence: u64,
}

/// What the session's sealed index holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub title: String,
    pub cwd: String,
    pub updated_ms: u64,
    pub head: Option<Head>,
    pub checkpoint: Option<Mark>,
    /// The machine that last took the session up, which `krowk hosts`
    /// lists it under; absent from an index written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostedOn>,
}

/// A host as it names itself in the sealed index: its device, and its
/// tailnet node when Tailscale was up as it took the session up. Sealed,
/// so neither the registry nor the relay learns an address from it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostedOn {
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<super::Tailnet>,
}

/// A checkpoint: the compacted context (every event logged to `log_offset`,
/// the session so far), the worktree it ran against, and the chunk before
/// it, so a reader starting here knows the log did not end sooner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub events: Vec<Value>,
    pub log_offset: u64,
    /// The commit the session's directory was at, when it is a repository.
    pub worktree: Option<String>,
    pub chunk: Option<Head>,
}

/// What one chunk of the log holds, once opened.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Chunk {
    Events { events: Vec<Value> },
    Checkpoint(Checkpoint),
}

/// How many chunks are read from storage at once.
const FETCH: usize = 8;

/// Why reading a session's chunks failed (`catch_up`), and whether reading
/// them again could help: a viewer joins again after a transient failure,
/// and stops at a permanent one.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadError {
    pub detail: String,
    /// Reading again reads the same: the session is gone (the registry
    /// answers 404 or 410), a chunk does not open under the keys this
    /// device holds (a key rotated since it was opened, a chain that does
    /// not hold) or is no chunk this build reads, or the log is not the one
    /// the host sealed (a prefix or another log: `session_rolled_back`).
    /// Anything else — the network, a registry error, a digest that did not
    /// match as read — is transient.
    pub permanent: bool,
    /// Permanent, and the log is not the one this device read or was told
    /// of: a prefix of it, an index older than its place, another chunk at
    /// a head it knows, or a chunk that does not open where it was served
    /// for any reason but a newer format or a rotated key (one changed,
    /// replayed, out of order, or not following the chunk before it). What
    /// the phone calls `session_rolled_back`.
    pub rolled_back: bool,
    /// The registry or storage call that failed, when one did: what tells
    /// the network out of reach from the rest (the phone's `Network`).
    pub registry: Option<krowk_api::Error>,
}

impl ReadError {
    pub fn transient(detail: impl Into<String>) -> ReadError {
        ReadError { detail: detail.into(), permanent: false, rolled_back: false, registry: None }
    }

    pub fn permanent(detail: impl Into<String>) -> ReadError {
        ReadError { detail: detail.into(), permanent: true, rolled_back: false, registry: None }
    }

    pub fn rolled_back(detail: impl Into<String>) -> ReadError {
        ReadError { detail: detail.into(), permanent: true, rolled_back: true, registry: None }
    }

    /// A registry or storage call that failed: permanent only when the
    /// session is gone.
    fn registry(e: krowk_api::Error) -> ReadError {
        let read = if matches!(e.status, 404 | 410) {
            ReadError::permanent(format!("the session, or a chunk of it, is gone from the registry ({e})"))
        } else {
            ReadError::transient(e.to_string())
        };
        ReadError { registry: Some(e), ..read }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl From<ReadError> for String {
    fn from(e: ReadError) -> String {
        e.detail
    }
}

/// The whole log as the registry holds it: its events and where it ends,
/// read from chunk 0, never from a checkpoint, with a checkpoint replacing
/// the events gathered before it. What a holder taking a session up chains
/// onto: public for the host's writer (`krowk_harness::sync::store`). It
/// holds the log to no head, so a registry serving a prefix goes unseen: a
/// reader that was not told the log's length goes through `attach` or
/// `catch_up`, which do (`Head::at_least`).
pub fn read_all(client: &Client, key: &SessionKeys, id: &str) -> Result<(Vec<Value>, ChunkReader, Option<Head>), String> {
    let mut reader = ChunkReader::new(key, crate::sync::uuid(id));
    let mut events = Vec::new();
    let head = read_from(client, id, &mut reader, None, &mut |c, _| {
        match c {
            Chunk::Events { events: e } => events.extend(e),
            Chunk::Checkpoint(cp) => events = cp.events,
        }
        Ok(())
    })?;
    Ok((events, reader, head))
}

/// Reads chunks from `reader`'s place to the end of what the registry lists,
/// handing each to `each` with its head, the chunks of a page fetched
/// `FETCH` at a time. What `attach`, `catch_up` and `read_all` (the host's
/// writer's) are built on. It holds what it read to no head: a caller
/// reading a session it was not told the length of goes through `attach`
/// or `catch_up`, which do (`Head::at_least`).
pub fn read_from(client: &Client, id: &str, reader: &mut ChunkReader, last: Option<Head>, each: &mut dyn FnMut(Chunk, Head) -> Result<(), String>) -> Result<Option<Head>, String> {
    read_chunks(client, id, reader, last, each).map_err(String::from)
}

/// `read_from`, its failure classified: what `attach` and `catch_up` read with. A
/// refusal of `each`'s is permanent.
fn read_chunks(client: &Client, id: &str, reader: &mut ChunkReader, mut last: Option<Head>, each: &mut dyn FnMut(Chunk, Head) -> Result<(), String>) -> Result<Option<Head>, ReadError> {
    let mut after = reader.next().checked_sub(1);
    loop {
        let page = client.list_chunks(id, after, 100).map_err(ReadError::registry)?;
        let wanted: Vec<_> = page.chunks.iter().filter(|c| c.index >= reader.next()).collect();
        // Each chunk is a round trip to storage, so a page is fetched
        // FETCH at a time and opened in order: an attach costs a few round
        // trips, not one a chunk (R-PERF-6).
        let mut fetched = Vec::with_capacity(wanted.len());
        for group in wanted.chunks(FETCH) {
            let got: Vec<_> = std::thread::scope(|s| group.iter().map(|c| s.spawn(move || client.read_chunk(c))).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap_or_else(|_| Err(krowk_api::fail("read_failed", "a chunk read panicked")))).collect());
            fetched.extend(got);
        }
        for (c, sealed) in wanted.iter().zip(fetched) {
            let sealed = sealed.map_err(ReadError::registry)?;
            // The listing's index is the registry's word: a chunk is held to
            // a head at the index it opens at, so one listed at another is a
            // gap or a relabelling that would hide the chunks between.
            if c.index != reader.next() {
                return Err(ReadError::rolled_back(format!("the registry listed chunk {} where chunk {} belongs: a gap, refused", c.index, reader.next())));
            }
            // The registry checked its digest as read. What this build or this device's keys cannot open yet (a newer
            // format, a key rotated since) is the same log; anything else
            // that does not open where it was served — changed, replayed,
            // reordered, after a gap or the final chunk, under a lower fence
            // or epoch, or not following the chunk before — is not.
            let plain = reader.open(&sealed).map_err(|e| {
                if sealed.first().is_some_and(|&v| v > e2e::CHUNK_V2) || e.0.contains(e2e::ROTATED_SINCE_OPENED) {
                    ReadError::permanent(e.to_string())
                } else {
                    ReadError::rolled_back(format!("chunk {}: {e}", c.index))
                }
            })?;
            let chunk: Chunk = serde_json::from_slice(&plain).map_err(|e| ReadError::permanent(format!("chunk {} holds no chunk this krowk reads: {e}", c.index)))?;
            let h = Head { index: c.index, digest: e2e::chunk_digest(&sealed) };
            last = Some(h);
            each(chunk, h).map_err(ReadError::permanent)?;
        }
        match page.next {
            Some(n) if !page.chunks.is_empty() => after = Some(n),
            _ => return Ok(last),
        }
    }
}

/// A session as a device attaching reads it at rest: the events to its
/// head, from its latest checkpoint.
#[derive(Debug, Clone, Default)]
pub struct Attached {
    pub index: Index,
    pub events: Vec<Value>,
    pub head: Option<Head>,
    /// Where the reading stopped, for reading the tail again later.
    pub next: u64,
    pub previous: [u8; 32],
    pub fence: u64,
    /// The key epoch of the last chunk read.
    pub epoch: u32,
    /// Every chunk read, for holding the log to a head it was told of.
    pub heads: Vec<Head>,
}

impl Attached {
    /// Where the reading stopped, to keep and `resume` from later; None
    /// before any chunk was read.
    pub fn place(&self) -> Option<ReadPlace> {
        let head = self.head?;
        Some(ReadPlace { next: self.next, previous: self.previous, fence: self.fence, epoch: self.epoch, head })
    }
}

/// Where a device's reading of a session stopped (`Attached::place`): the
/// next chunk's index, the digest it must bind, the fence and key epoch it
/// may not go below, and the head read. Kept with what was read, it lets
/// the next attach read only the chunks after it (`resume`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadPlace {
    pub next: u64,
    #[serde(with = "hex32")]
    pub previous: [u8; 32],
    pub fence: u64,
    pub epoch: u32,
    pub head: Head,
}

impl ReadPlace {
    /// Refuses a place no reading stopped at: the head is the last chunk
    /// read, so the next is the one after it and binds its digest.
    pub fn check(&self) -> Result<(), String> {
        if self.head.index.checked_add(1) != Some(self.next) || self.head.digest != self.previous {
            return Err(format!("a read place at chunk {} with its head at chunk {} is not one a reading stopped at", self.next, self.head.index));
        }
        Ok(())
    }
}

/// A session's keys, out of the session record: only a record signed by a
/// device the verified list has held — listed now, `from` `Listed`, when a
/// host takes it up to write under (`session_record::verify`) — and then
/// opened with the generations this device holds. An unsigned record, one
/// signed by a device the list never held, and one sealed some way other
/// than to the user key, are refused before anything is unwrapped.
pub fn open_session_key(s: &krowk_api::sync::SyncSession, id: &str, keys: &UserKeys, chain: &Chain, from: session_record::Signer) -> Result<SessionKeys, String> {
    let raw = crate::sync::uuid(id);
    let wrapped = e2e::unhex(&s.wrapped_key).ok_or("the session's wrapped key is not hex")?;
    let seal = if s.seal.is_empty() { session_record::SEAL_USER } else { s.seal.as_str() };
    let signer = e2e::DeviceId::parse(&s.signer).ok_or_else(|| format!("the registry's record of session {id} names no signer — refused, since anyone could have made it"))?;
    let signature = e2e::unhex(&s.record_signature).ok_or_else(|| format!("the registry's record of session {id} carries no signature — refused"))?;
    session_record::verify(&raw, &wrapped, seal, signer, &signature, chain, from).map_err(|e| e.to_string())?;
    e2e::unwrap_session_keys(&wrapped, &raw, keys).map_err(|e| e.to_string())
}

/// A synced session as a listing shows it (`krowk sessions`, the phone's
/// session list), from its sealed index.
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: String,
    pub index: Index,
    /// The device holding the session's lease, when one does.
    pub holder: Option<String>,
    /// The device whose signature the session's record carries.
    pub signer: String,
}

/// Every synced session this device can open, most recently written first,
/// and how many it cannot. One whose record no device the verified list
/// has held signed, or whose key or index does not open under the user
/// keys this device holds — one sealed under a generation newer than it
/// has, say — is left out and counted, never an error: what fails it is a
/// registry call, and its error is that call's. Blocking: a page of
/// the listing and then one `show` a session, since a listing leaves the
/// sealed index out.
pub fn list(api: &Client, keys: &UserKeys, chain: &Chain) -> Result<(Vec<Listed>, usize), krowk_api::Error> {
    let mut out = Vec::new();
    let mut unreadable = 0;
    let mut before = String::new();
    loop {
        let page = api.list_sync_sessions(&before, 100)?;
        for s in &page.sessions {
            // A listing leaves the index out; `show` has it.
            let full = api.show_sync_session(&s.id)?;
            match open_session_key(&full, &s.id, keys, chain, session_record::Signer::EverHeld).and_then(|k| open_index(&k, &s.id, &full.sealed_index)) {
                Ok(index) => out.push(Listed { id: s.id.clone(), index, holder: full.lease.map(|l| l.device), signer: full.signer.clone() }),
                Err(_) => unreadable += 1,
            }
        }
        if page.next.is_empty() {
            return Ok((out, unreadable));
        }
        before = page.next;
    }
}

/// Opens a session's sealed index: under the current key, or an older
/// one's when a rotation's key landed and its index write did not — the
/// holder's next index write seals it under the current one.
pub fn open_index(key: &SessionKeys, id: &str, sealed_hex: &str) -> Result<Index, String> {
    if sealed_hex.is_empty() {
        return Ok(Index::default());
    }
    let blob = e2e::unhex(sealed_hex).ok_or("the session's index is not hex")?;
    let session = crate::sync::uuid(id);
    let mut opened = e2e::open_session_index(&blob, &session, key.current());
    for epoch in (0..key.epoch()).rev() {
        if opened.is_ok() {
            break;
        }
        opened = e2e::open_session_index(&blob, &session, key.at(epoch).expect("within the ring"));
    }
    let plain = opened.map_err(|e| e.to_string())?;
    serde_json::from_slice(&plain).map_err(|e| format!("the session's index is not one this krowk reads: {e}"))
}

/// Attaches from the latest checkpoint and reads the tail after it, then
/// holds what was read to the index's head and to `known`. A log short of
/// either, or holding another chunk there, is a rolled-back `ReadError`.
pub fn attach(client: &Client, key: &SessionKeys, id: &str, index: Index, known: Option<Head>) -> Result<Attached, ReadError> {
    let session = crate::sync::uuid(id);
    let mut reader = match index.checkpoint {
        Some(m) => ChunkReader::resume(key, session, m.index, m.previous, m.fence, 0),
        None => ChunkReader::new(key, session),
    };
    let mut events = Vec::new();
    let mut heads = Vec::new();
    let head = read_chunks(client, id, &mut reader, None, &mut |c, h| {
        match c {
            Chunk::Events { events: e } => events.extend(e),
            Chunk::Checkpoint(cp) => {
                // The head before the checkpoint, as the host sealed it in.
                heads.extend(cp.chunk);
                events = cp.events;
            }
        }
        heads.push(h);
        Ok(())
    })?;
    Head::at_least(&heads, index.head).map_err(ReadError::rolled_back)?;
    Head::at_least(&heads, known).map_err(ReadError::rolled_back)?;
    Ok(Attached { next: reader.next(), previous: reader.previous(), fence: reader.fence(), epoch: reader.epoch(), index, events, head, heads })
}

/// Reads the tail again from where `a` stopped, until it reaches `known`.
/// A log that stops short of `known`, or holds another chunk there, is a
/// permanent `ReadError`, as `session_rolled_back` is to the phone.
pub fn catch_up(client: &Client, key: &SessionKeys, id: &str, a: &mut Attached, known: Option<Head>) -> Result<Vec<Value>, ReadError> {
    let mut reader = ChunkReader::resume(key, crate::sync::uuid(id), a.next, a.previous, a.fence, a.epoch);
    let mut fresh = Vec::new();
    let mut heads = std::mem::take(&mut a.heads);
    let head = read_chunks(client, id, &mut reader, a.head, &mut |c, h| {
        if let Chunk::Events { events } = c {
            fresh.extend(events);
        }
        heads.push(h);
        Ok(())
    });
    a.heads = heads;
    let head = head?;
    Head::at_least(&a.heads, known).map_err(ReadError::rolled_back)?;
    a.head = head;
    a.next = reader.next();
    a.previous = reader.previous();
    a.fence = reader.fence();
    a.epoch = reader.epoch();
    a.events.extend(fresh.iter().cloned());
    Ok(fresh)
}

/// Attaches from `place`, where this device's reading stopped before,
/// rather than from the checkpoint: only the chunks after it are read, and
/// only their events are in what it returns. They are held to the same
/// chain (the first must follow `place`'s head), then to the index's head
/// and to `known`. A chunk at `place.next` that does not follow it is
/// another log, refused for good. An index older than `place` is only
/// behind (a holder writes the chunk, then the index, and the index write
/// can fail and be owed) while the registry still holds the place's own
/// chunk, or serves the log on past it; when it holds neither, it rolled
/// back past what this device read, refused for good.
pub fn resume(client: &Client, key: &SessionKeys, id: &str, index: Index, place: ReadPlace, known: Option<Head>) -> Result<Attached, ReadError> {
    place.check().map_err(ReadError::permanent)?;
    let behind = match index.head {
        Some(h) if h.index > place.head.index || h == place.head => None,
        Some(h) if h.index == place.head.index => return Err(ReadError::rolled_back(format!("the session's index names another chunk {} than the one this device read: the registry served another log", h.index))),
        h => Some(format!("the session's index ends at chunk {}, where this device read to chunk {}, and the registry no longer holds that chunk: rolled back past what this device read, refused", h.map_or(-1, |h| h.index as i64), place.head.index)),
    };
    // Only what is read from here is held to a head: before it, the chain
    // this device read binds it.
    let sealed = if behind.is_some() { None } else { index.head };
    let mut a = Attached { index, events: Vec::new(), head: Some(place.head), next: place.next, previous: place.previous, fence: place.fence, epoch: place.epoch, heads: vec![place.head] };
    catch_up(client, key, id, &mut a, sealed)?;
    if let Some(e) = behind.filter(|_| a.next == place.next) {
        // Nothing after the place: the place's own chunk, as the registry
        // lists it, says whether the index is behind or the log went back.
        let listed = client.list_chunks(id, place.head.index.checked_sub(1), 1).map_err(ReadError::registry)?;
        if !listed.chunks.first().is_some_and(|c| c.index == place.head.index && c.checksum == e2e::hex(&place.head.digest)) {
            return Err(ReadError::rolled_back(e));
        }
    }
    Head::at_least(&a.heads, known).map_err(ReadError::rolled_back)?;
    Ok(a)
}

/// The image whose sha256 is `sha256` (lowercase hex, as the log names it)
/// from session `id`'s image objects (R-IMG-11): fetched by its keyed name
/// (`e2e::image_name`), opened under the session's keys and held to that
/// hash. `None` while the registry holds no such object — the host has not
/// put it yet, or never will (an image logged before sync, or the session
/// gone) — which a device shows as its `[Image #N]` placeholder and asks
/// for again later, never of the host. A hash that is not 64 hex digits, or
/// an object that does not open as that image, is permanent; the network
/// and the registry's errors are transient, as for the chunks.
pub fn fetch_image(client: &Client, keys: &SessionKeys, id: &str, sha256: &str) -> Result<Option<Vec<u8>>, ReadError> {
    let hash: [u8; 32] = match e2e::unhex(sha256).map(<[u8; 32]>::try_from) {
        Some(Ok(h)) => h,
        _ => return Err(ReadError::permanent(format!("{sha256:?} is not an image's sha256: 64 hex digits"))),
    };
    let blob = match client.get_image_object(id, &e2e::image_name(keys, &hash)) {
        Ok(b) => b,
        Err(e) if e.status == 404 => return Ok(None),
        Err(e) => return Err(ReadError { registry: Some(e.clone()), ..ReadError::transient(e.to_string()) }),
    };
    e2e::open_image(&blob, &crate::sync::uuid(id), &hash, keys).map(Some).map_err(|e| ReadError::permanent(e.0))
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::e2e::hex(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        crate::e2e::unhex(&s).and_then(|v| v.try_into().ok()).ok_or_else(|| serde::de::Error::custom("32 hex bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(i: u64, d: u8) -> Option<Head> {
        Some(Head { index: i, digest: [d; 32] })
    }

    fn log(n: u64, d: u8) -> Vec<Head> {
        (0..n).map(|i| Head { index: i, digest: [d; 32] }).collect()
    }

    /// The prefix check: a log that stops short of a head this device was
    /// told of, or holds another chunk at that head's index — reached or
    /// passed — is refused.
    #[test]
    fn r_sync_1_a_log_short_of_the_known_head_is_a_prefix_and_refused() {
        assert!(Head::at_least(&log(5, 1), h(4, 1)).is_ok());
        assert!(Head::at_least(&log(6, 1), h(4, 1)).is_ok());
        assert!(Head::at_least(&log(4, 1), h(4, 1)).unwrap_err().contains("prefix"));
        assert!(Head::at_least(&[], h(0, 1)).unwrap_err().contains("prefix"));
        assert!(Head::at_least(&log(5, 9), h(4, 1)).is_err(), "another chunk at the head");
        assert!(Head::at_least(&log(8, 9), h(4, 1)).is_err(), "a longer log that forked before the head");
        assert!(Head::at_least(&[], None).is_ok());
    }

    /// Ticket 115: a registry or storage failure is permanent only when the
    /// session is gone (404, 410); the network, a 5xx, a 429, a refusal
    /// and a digest that did not match as read are worth reading again.
    #[test]
    fn r_client_3_a_read_fails_for_good_only_when_the_session_is_gone() {
        let refused = |status: u16, code: &str| ReadError::registry(krowk_api::Error { status, body: [("error".to_string(), serde_json::json!(code))].into() });
        assert!(refused(404, "not_found").permanent);
        assert!(refused(410, "gone").permanent);
        assert!(refused(404, "not_found").detail.contains("is gone from the registry"));
        for (status, code) in [(0, "network_unreachable"), (503, "http_503"), (429, "rate_limited"), (401, "unauthorized"), (0, "checksum_mismatch")] {
            let e = refused(status, code);
            assert!(!e.permanent && e.detail == code, "{status} {code}: {e:?}");
        }
        assert!(!ReadError::registry(krowk_api::fail("read_failed", "a chunk read panicked")).permanent);
        assert_eq!(String::from(ReadError::permanent("a prefix, refused")), "a prefix, refused");
    }

    /// An index whose rotation's key landed and whose next index write did
    /// not is still sealed under the older epoch: it opens, newest key
    /// first, then each older one, and under no key the ring lacks.
    #[test]
    fn r_client_1_open_index_opens_under_an_older_epoch() {
        let id = "0b1d2e3f-4a5b-4c6d-8e7f-901a2b3c4d5e";
        let session = crate::sync::uuid(id);
        let old = e2e::SessionKeys::from(e2e::SessionKey::generate());
        let index = Index { title: "fix the build".into(), cwd: "/src".into(), updated_ms: 7, head: h(3, 2), checkpoint: None, host: None };
        let seal = |k: &e2e::SessionKey| e2e::hex(&e2e::seal_session_index(k, &session, &serde_json::to_vec(&index).unwrap()));
        let sealed = seal(old.current());
        let rotated = old.rotated().unwrap();
        assert_eq!(rotated.epoch(), 1);
        assert_eq!(open_index(&rotated, id, &sealed).unwrap(), index);
        assert_eq!(open_index(&old, id, &sealed).unwrap(), index);
        // Rotated twice: epoch 0 is reached past epoch 1, and an index
        // sealed under epoch 1 opens too.
        let twice = rotated.rotated().unwrap();
        assert_eq!(twice.epoch(), 2);
        assert_eq!(open_index(&twice, id, &sealed).unwrap(), index);
        assert_eq!(open_index(&twice, id, &seal(twice.at(1).unwrap())).unwrap(), index);
        assert!(open_index(&old, id, &seal(twice.current())).is_err(), "a newer epoch than the ring holds");
        let other = e2e::SessionKeys::from(e2e::SessionKey::generate()).rotated().unwrap();
        assert!(open_index(&other, id, &sealed).is_err(), "no key of the ring");
        assert!(open_index(&twice, "1b1d2e3f-4a5b-4c6d-8e7f-901a2b3c4d5e", &sealed).is_err(), "another session's index");
        assert_eq!(open_index(&twice, id, "").unwrap(), Index::default());
    }
}
