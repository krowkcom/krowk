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
use krowk_client::e2e::{self, ChunkReader, ChunkSealer, SessionKey};
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
    /// serving a prefix, or an index older than the log.
    pub fn at_least(read: Option<Head>, known: Option<Head>) -> Result<(), String> {
        let Some(k) = known else { return Ok(()) };
        match read {
            Some(r) if r.index > k.index => Ok(()),
            Some(r) if r.index == k.index && r.digest == k.digest => Ok(()),
            Some(r) if r.index == k.index => Err(format!("chunk {} is not the one the session's host sealed: the registry served another log", r.index)),
            _ => Err(format!("the registry served the log only to chunk {}, where the session's host has sealed to chunk {}: a prefix, refused", read.map_or(-1, |r| r.index as i64), k.index)),
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

fn api(e: krowk_api::Error) -> String {
    e.to_string()
}

/// The lease holder's writer: seals and puts each chunk, and keeps the
/// sealed index pointing at the head and the latest checkpoint.
pub struct Writer {
    client: std::sync::Arc<Client>,
    key: SessionKey,
    id: String,
    session: [u8; 16],
    wrapped: String,
    sealer: ChunkSealer,
    fence: u64,
    pub index: Index,
    /// Every event so far: the next checkpoint's compacted context.
    events: Vec<Value>,
    pending: Vec<Value>,
}

impl Writer {
    /// Takes up a log: reads it from chunk 0 to its end, so what it chains
    /// onto is the whole log (crypto.md → chunks), under lease `fence`.
    pub fn take_up(client: std::sync::Arc<Client>, key: SessionKey, id: &str, wrapped: String, index: Index, fence: u64) -> Result<Writer, String> {
        let session = crate::daemon::ws::uuid(id);
        let mut reader = ChunkReader::new(&key, session);
        let mut events = Vec::new();
        let mut index = index;
        index.head = read_from(&client, id, &mut reader, None, &mut |c| {
            match c {
                Chunk::Events { events: e } => events.extend(e),
                Chunk::Checkpoint(cp) => events = cp.events,
            }
            Ok(())
        })?;
        let sealer = ChunkSealer::new(&key, session, reader.next(), reader.previous(), fence);
        Ok(Writer { client, key, id: id.to_string(), session, wrapped, sealer, fence, index, events, pending: Vec::new() })
    }

    /// The lease moved on (a lapse and a new acquire): what follows is
    /// written under the new fence, chained onto the same log.
    pub fn refence(&mut self, fence: u64) {
        if fence != self.fence {
            self.sealer = ChunkSealer::new(&self.key, self.session, self.sealer.next(), self.previous(), fence);
            self.fence = fence;
        }
    }

    fn previous(&self) -> [u8; 32] {
        self.index.head.map_or(e2e::NO_PREVIOUS_CHUNK, |h| h.digest)
    }

    pub fn head(&self) -> Option<Head> {
        self.index.head
    }

    /// How many events the log holds, those not yet in a chunk included.
    pub fn log_offset(&self) -> u64 {
        (self.events.len() + self.pending.len()) as u64
    }

    pub fn push(&mut self, event: Value) {
        self.pending.push(event);
    }

    fn put(&mut self, chunk: &Chunk, token: &str) -> Result<Head, String> {
        let plain = serde_json::to_vec(chunk).map_err(|e| e.to_string())?;
        let index = self.sealer.next();
        let sealed = self.sealer.seal(&plain, false).map_err(|e| e.to_string())?;
        self.client.put_chunk(&self.id, index, &sealed, token).map_err(api)?;
        let head = Head { index, digest: e2e::chunk_digest(&sealed) };
        self.index.head = Some(head);
        Ok(head)
    }

    /// Puts the events waiting as one chunk, and the index after it.
    pub fn flush(&mut self, token: &str) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let events = std::mem::take(&mut self.pending);
        self.put(&Chunk::Events { events: events.clone() }, token)?;
        self.events.extend(events);
        self.write_index(token)
    }

    /// Cuts a checkpoint: what is waiting first, then the session so far as
    /// one chunk, which the index then names.
    pub fn checkpoint(&mut self, worktree: Option<String>, token: &str) -> Result<(), String> {
        self.flush(token)?;
        let before = self.index.head;
        let mark = Mark { index: self.sealer.next(), previous: self.previous(), fence: self.fence };
        let cp = Checkpoint { events: self.events.clone(), log_offset: self.events.len() as u64, worktree, chunk: before };
        self.put(&Chunk::Checkpoint(cp), token)?;
        self.index.checkpoint = Some(mark);
        self.write_index(token)
    }

    fn write_index(&mut self, token: &str) -> Result<(), String> {
        self.index.updated_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let plain = serde_json::to_vec(&self.index).map_err(|e| e.to_string())?;
        let sealed = e2e::hex(&e2e::seal_session_index(&self.key, &self.session, &plain));
        self.client.put_sync_session(&self.id, &self.wrapped, Some(&sealed), Some(token)).map_err(api)?;
        Ok(())
    }
}

/// Reads chunks from `reader`'s place to the end of what the registry lists.
fn read_from(client: &Client, id: &str, reader: &mut ChunkReader, mut last: Option<Head>, each: &mut dyn FnMut(Chunk) -> Result<(), String>) -> Result<Option<Head>, String> {
    let mut after = reader.next().checked_sub(1);
    loop {
        let page = client.list_chunks(id, after, 100).map_err(api)?;
        for c in &page.chunks {
            if c.index < reader.next() {
                continue;
            }
            let sealed = client.read_chunk(c).map_err(api)?;
            let plain = reader.open(&sealed).map_err(|e| e.to_string())?;
            let chunk: Chunk = serde_json::from_slice(&plain).map_err(|e| format!("chunk {} holds no chunk this krowk reads: {e}", c.index))?;
            last = Some(Head { index: c.index, digest: e2e::chunk_digest(&sealed) });
            each(chunk)?;
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
}

/// Opens a session's sealed index.
pub fn open_index(key: &SessionKey, id: &str, sealed_hex: &str) -> Result<Index, String> {
    if sealed_hex.is_empty() {
        return Ok(Index::default());
    }
    let blob = e2e::unhex(sealed_hex).ok_or("the session's index is not hex")?;
    let plain = e2e::open_session_index(&blob, &crate::daemon::ws::uuid(id), key).map_err(|e| e.to_string())?;
    serde_json::from_slice(&plain).map_err(|e| format!("the session's index is not one this krowk reads: {e}"))
}

/// Attaches from the latest checkpoint and reads the tail after it, then
/// holds what was read to the index's head and to `known`.
pub fn attach(client: &Client, key: &SessionKey, id: &str, index: Index, known: Option<Head>) -> Result<Attached, String> {
    let session = crate::daemon::ws::uuid(id);
    let mut reader = match index.checkpoint {
        Some(m) => ChunkReader::resume(key, session, m.index, m.previous, m.fence),
        None => ChunkReader::new(key, session),
    };
    let mut events = Vec::new();
    let head = read_from(client, id, &mut reader, None, &mut |c| {
        match c {
            Chunk::Events { events: e } => events.extend(e),
            Chunk::Checkpoint(cp) => {
                Head::at_least(cp.chunk, None)?;
                events = cp.events;
            }
        }
        Ok(())
    })?;
    Head::at_least(head, index.head)?;
    Head::at_least(head, known)?;
    Ok(Attached { next: reader.next(), previous: reader.previous(), fence: reader.fence(), index, events, head })
}

/// Reads the tail again from where `a` stopped, until it reaches `known`.
pub fn catch_up(client: &Client, key: &SessionKey, id: &str, a: &mut Attached, known: Option<Head>) -> Result<Vec<Value>, String> {
    let mut reader = ChunkReader::resume(key, crate::daemon::ws::uuid(id), a.next, a.previous, a.fence);
    let mut fresh = Vec::new();
    let head = read_from(client, id, &mut reader, a.head, &mut |c| {
        match c {
            Chunk::Events { events } => fresh.extend(events),
            Chunk::Checkpoint(_) => {}
        }
        Ok(())
    })?;
    Head::at_least(head, known)?;
    a.head = head;
    a.next = reader.next();
    a.previous = reader.previous();
    a.fence = reader.fence();
    a.events.extend(fresh.iter().cloned());
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(i: u64, d: u8) -> Option<Head> {
        Some(Head { index: i, digest: [d; 32] })
    }

    /// The prefix check: a log that stops short of a head this device was
    /// told of, or reaches it with another chunk, is refused.
    #[test]
    fn r_sync_1_a_log_short_of_the_known_head_is_a_prefix_and_refused() {
        assert!(Head::at_least(h(4, 1), h(4, 1)).is_ok());
        assert!(Head::at_least(h(5, 2), h(4, 1)).is_ok());
        assert!(Head::at_least(h(3, 1), h(4, 1)).unwrap_err().contains("prefix"));
        assert!(Head::at_least(None, h(0, 1)).unwrap_err().contains("prefix"));
        assert!(Head::at_least(h(4, 9), h(4, 1)).is_err());
        assert!(Head::at_least(None, None).is_ok());
    }
}
