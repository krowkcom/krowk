//! A synced session at rest (engineering/harness.md → Sync): its sealed
//! index, the chunks of its log and the checkpoints among them, and reading
//! them back from a checkpoint on another device.
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

use krowk_api::Client;
use krowk_client::e2e::{self, ChunkReader, ChunkSealer, SessionKeys};
use krowk_client::device_chain::Chain;
use krowk_client::session_record;
use krowk_client::user_key::UserKeys;
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
    pub tailnet: Option<super::tailscale::Tailnet>,
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Chunk {
    Events { events: Vec<Value> },
    Checkpoint(Checkpoint),
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&krowk_client::e2e::hex(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        krowk_client::e2e::unhex(&s).and_then(|v| v.try_into().ok()).ok_or_else(|| serde::de::Error::custom("32 hex bytes"))
    }
}

/// How many chunks are read from storage at once.
const FETCH: usize = 8;

fn api(e: krowk_api::Error) -> String {
    e.to_string()
}

/// A chunk sealed and not yet stored: kept, bytes and all, and put again
/// under the same index, digest and Idempotency-Key until the registry has
/// it. Sealing moved the chain on, so this chunk is the only one that may
/// come next; a chunk never sealed past a chunk that was never stored.
struct Staged {
    index: u64,
    sealed: Vec<u8>,
    key: String,
    /// The events it holds, taken out of `pending`: they rejoin it should
    /// the chunk have to be sealed again elsewhere in the log.
    events: Vec<Value>,
    /// Set for a checkpoint: where the index is to point once it is stored.
    mark: Option<Mark>,
}

/// The lease holder's writer: seals and puts each chunk, and keeps the
/// sealed index pointing at the head and the latest checkpoint. Every
/// write is a transaction: the events of a turn stay with the writer until
/// the chunk holding them is stored, however many attempts that takes.
pub struct Writer {
    client: std::sync::Arc<Client>,
    key: SessionKeys,
    id: String,
    session: [u8; 16],
    wrapped: String,
    sealer: ChunkSealer,
    fence: u64,
    pub index: Index,
    /// Every event stored so far: the next checkpoint's compacted context.
    events: Vec<Value>,
    pending: Vec<Value>,
    staged: Option<Staged>,
    /// The index write failed and is owed.
    index_owed: bool,
}

/// The whole log as the registry holds it: its events and where it ends.
fn read_all(client: &Client, key: &SessionKeys, id: &str) -> Result<(Vec<Value>, ChunkReader, Option<Head>), String> {
    let mut reader = ChunkReader::new(key, crate::daemon::ws::uuid(id));
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

impl Writer {
    /// Takes up a log: reads it from chunk 0 to its end, so what it chains
    /// onto is the whole log (crypto.md → chunks), under lease `fence`.
    pub fn take_up(client: std::sync::Arc<Client>, key: SessionKeys, id: &str, wrapped: String, index: Index, fence: u64) -> Result<Writer, String> {
        let session = crate::daemon::ws::uuid(id);
        let (events, reader, head) = read_all(&client, &key, id)?;
        let mut index = index;
        index.head = head;
        let sealer = ChunkSealer::new(&key, session, reader.next(), reader.previous(), fence);
        Ok(Writer { client, key, id: id.to_string(), session, wrapped, sealer, fence, index, events, pending: Vec::new(), staged: None, index_owed: false })
    }

    /// The lease moved on (a lapse and a new acquire): the log is read
    /// again, since another holder may have written in between, and what
    /// follows chains onto the log as the registry holds it — never onto
    /// this writer's memory of it. A staged chunk the log already holds is
    /// done; one it does not hold and cannot follow is dropped and its
    /// events sealed again after the registry's head.
    pub fn refence(&mut self, fence: u64) -> Result<(), String> {
        if fence == self.fence {
            return Ok(());
        }
        let (events, reader, head) = read_all(&self.client, &self.key, &self.id)?;
        let staged_head = self.staged.as_ref().map(|s| Head { index: s.index, digest: e2e::chunk_digest(&s.sealed) });
        if head.is_some() && head == staged_head {
            self.commit();
        } else if head != self.index.head {
            // Another holder wrote: this writer's unstored events go after it.
            if let Some(s) = self.staged.take() {
                let mut again = s.events;
                again.append(&mut self.pending);
                self.pending = again;
            }
            self.events = events;
            self.index.head = head;
            self.sealer = ChunkSealer::new(&self.key, self.session, reader.next(), reader.previous(), fence);
        } else if let Some(s) = self.staged.as_mut() {
            // The staged chunk still follows the log: put it under the new
            // lease, with a key of its own (the old lease's declare is the
            // registry's to replace).
            s.key = krowk_api::client::idempotency_key().map_err(api)?;
        }
        if self.staged.is_none() {
            self.sealer = ChunkSealer::new(&self.key, self.session, self.sealer.next(), self.previous(), fence);
        }
        self.fence = fence;
        Ok(())
    }

    fn previous(&self) -> [u8; 32] {
        self.index.head.map_or(e2e::NO_PREVIOUS_CHUNK, |h| h.digest)
    }

    pub fn head(&self) -> Option<Head> {
        self.index.head
    }

    /// How many events the log holds, those not yet in a chunk included.
    pub fn log_offset(&self) -> u64 {
        (self.events.len() + self.staged.as_ref().map_or(0, |s| s.events.len()) + self.pending.len()) as u64
    }

    /// Whether a write is owed: a chunk staged or the index unwritten.
    pub fn owes(&self) -> bool {
        self.staged.is_some() || self.index_owed
    }

    pub fn push(&mut self, event: Value) {
        self.pending.push(event);
    }

    fn stage(&mut self, chunk: &Chunk, events: Vec<Value>, mark: Option<Mark>) -> Result<(), String> {
        let plain = serde_json::to_vec(chunk).map_err(|e| e.to_string())?;
        let index = self.sealer.next();
        let sealed = self.sealer.seal(&plain, false).map_err(|e| e.to_string())?;
        let key = krowk_api::client::idempotency_key().map_err(api)?;
        self.staged = Some(Staged { index, sealed, key, events, mark });
        Ok(())
    }

    /// The staged chunk is stored: its events join the log.
    fn commit(&mut self) {
        let Some(s) = self.staged.take() else { return };
        self.index.head = Some(Head { index: s.index, digest: e2e::chunk_digest(&s.sealed) });
        if let Some(m) = s.mark {
            self.index.checkpoint = Some(m);
        } else {
            self.events.extend(s.events);
        }
        self.index_owed = true;
    }

    /// Puts the staged chunk. A failure whose answer may have been lost is
    /// checked against the registry's listing: the chunk may be there.
    fn put_staged(&mut self, token: &str) -> Result<(), String> {
        let Some(s) = self.staged.as_ref() else { return Ok(()) };
        let put = self.client.put_chunk_keyed(&self.id, s.index, &s.sealed, token, &s.key);
        if let Err(e) = put {
            let sum = e2e::hex(&e2e::chunk_digest(&s.sealed));
            let listed = self.client.list_chunks(&self.id, s.index.checked_sub(1), 1).map_err(api)?;
            if !listed.chunks.first().is_some_and(|c| c.index == s.index && c.checksum == sum && c.state == "ready") {
                return Err(api(e));
            }
        }
        self.commit();
        Ok(())
    }

    /// Puts what is owed — a staged chunk, then the events waiting as one
    /// chunk — and the index after it. On a failure nothing is lost: the
    /// next flush puts the same chunk again.
    pub fn flush(&mut self, token: &str) -> Result<(), String> {
        self.put_staged(token)?;
        if !self.pending.is_empty() {
            let events = std::mem::take(&mut self.pending);
            self.stage(&Chunk::Events { events: events.clone() }, events, None)?;
            self.put_staged(token)?;
        }
        if self.index_owed {
            self.write_index(token)?;
        }
        Ok(())
    }

    /// Cuts a checkpoint: what is waiting first, then the session so far as
    /// one chunk, which the index then names.
    pub fn checkpoint(&mut self, worktree: Option<String>, token: &str) -> Result<(), String> {
        self.flush(token)?;
        let before = self.index.head;
        let mark = Mark { index: self.sealer.next(), previous: self.previous(), fence: self.fence };
        let cp = Checkpoint { events: self.events.clone(), log_offset: self.events.len() as u64, worktree, chunk: before };
        self.stage(&Chunk::Checkpoint(cp), Vec::new(), Some(mark))?;
        self.flush(token)
    }

    fn write_index(&mut self, token: &str) -> Result<(), String> {
        self.index.updated_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let plain = serde_json::to_vec(&self.index).map_err(|e| e.to_string())?;
        let sealed = e2e::hex(&e2e::seal_session_index(self.key.current(), &self.session, &plain));
        self.client.put_sync_session(&self.id, &self.wrapped, None, Some(&sealed), Some(token)).map_err(api)?;
        self.index_owed = false;
        Ok(())
    }
}

/// Reads chunks from `reader`'s place to the end of what the registry lists.
fn read_from(client: &Client, id: &str, reader: &mut ChunkReader, mut last: Option<Head>, each: &mut dyn FnMut(Chunk, Head) -> Result<(), String>) -> Result<Option<Head>, String> {
    let mut after = reader.next().checked_sub(1);
    loop {
        let page = client.list_chunks(id, after, 100).map_err(api)?;
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
            let sealed = sealed.map_err(api)?;
            let plain = reader.open(&sealed).map_err(|e| e.to_string())?;
            let chunk: Chunk = serde_json::from_slice(&plain).map_err(|e| format!("chunk {} holds no chunk this krowk reads: {e}", c.index))?;
            let h = Head { index: c.index, digest: e2e::chunk_digest(&sealed) };
            last = Some(h);
            each(chunk, h)?;
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

/// A session's keys, out of the session record: only a record signed by a
/// device the verified list has held — listed now, `from` `Listed`, when a
/// host takes it up to write under (`session_record::verify`) — and then
/// opened with the generations this device holds. An unsigned record, one
/// signed by a device the list never held, and one sealed some way other
/// than to the user key, are refused before anything is unwrapped.
pub fn open_session_key(s: &krowk_api::sync::SyncSession, id: &str, keys: &UserKeys, chain: &Chain, from: session_record::Signer) -> Result<SessionKeys, String> {
    let raw = crate::daemon::ws::uuid(id);
    let wrapped = e2e::unhex(&s.wrapped_key).ok_or("the session's wrapped key is not hex")?;
    let seal = if s.seal.is_empty() { session_record::SEAL_USER } else { s.seal.as_str() };
    let signer = e2e::DeviceId::parse(&s.signer).ok_or_else(|| format!("the registry's record of session {id} names no signer — refused, since anyone could have made it"))?;
    let signature = e2e::unhex(&s.record_signature).ok_or_else(|| format!("the registry's record of session {id} carries no signature — refused"))?;
    session_record::verify(&raw, &wrapped, seal, signer, &signature, chain, from).map_err(|e| e.to_string())?;
    e2e::unwrap_session_keys(&wrapped, &raw, keys).map_err(|e| e.to_string())
}

/// Opens a session's sealed index: under the current key, or an older
/// one's when a rotation's key landed and its index write did not — the
/// holder's next index write seals it under the current one.
pub fn open_index(key: &SessionKeys, id: &str, sealed_hex: &str) -> Result<Index, String> {
    if sealed_hex.is_empty() {
        return Ok(Index::default());
    }
    let blob = e2e::unhex(sealed_hex).ok_or("the session's index is not hex")?;
    let session = crate::daemon::ws::uuid(id);
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
/// holds what was read to the index's head and to `known`.
pub fn attach(client: &Client, key: &SessionKeys, id: &str, index: Index, known: Option<Head>) -> Result<Attached, String> {
    let session = crate::daemon::ws::uuid(id);
    let mut reader = match index.checkpoint {
        Some(m) => ChunkReader::resume(key, session, m.index, m.previous, m.fence, 0),
        None => ChunkReader::new(key, session),
    };
    let mut events = Vec::new();
    let mut heads = Vec::new();
    let head = read_from(client, id, &mut reader, None, &mut |c, h| {
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
    Head::at_least(&heads, index.head)?;
    Head::at_least(&heads, known)?;
    Ok(Attached { next: reader.next(), previous: reader.previous(), fence: reader.fence(), epoch: reader.epoch(), index, events, head, heads })
}

/// Reads the tail again from where `a` stopped, until it reaches `known`.
pub fn catch_up(client: &Client, key: &SessionKeys, id: &str, a: &mut Attached, known: Option<Head>) -> Result<Vec<Value>, String> {
    let mut reader = ChunkReader::resume(key, crate::daemon::ws::uuid(id), a.next, a.previous, a.fence, a.epoch);
    let mut fresh = Vec::new();
    let mut heads = std::mem::take(&mut a.heads);
    let head = read_from(client, id, &mut reader, a.head, &mut |c, h| {
        if let Chunk::Events { events } = c {
            fresh.extend(events);
        }
        heads.push(h);
        Ok(())
    });
    a.heads = heads;
    let head = head?;
    Head::at_least(&a.heads, known)?;
    a.head = head;
    a.next = reader.next();
    a.previous = reader.previous();
    a.fence = reader.fence();
    a.epoch = reader.epoch();
    a.events.extend(fresh.iter().cloned());
    Ok(fresh)
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
}
