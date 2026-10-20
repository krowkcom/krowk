//! A synced session at rest (engineering/harness.md → Sync): the lease
//! holder's writer, which seals and stores the chunks of its log and the
//! sealed index. The shapes of the index and the chunks, and reading them
//! back (`attach`, `catch_up`, and the key and index openers), are every
//! client's (R-CLIENT-1): `krowk_client::sync::store` declares them, and
//! they are re-exported here so the harness's paths do not move.

use krowk_api::Client;
use krowk_client::e2e::{self, ChunkSealer, SessionKeys};
use krowk_client::sync::store::read_all;
use serde_json::Value;

/// The shapes of the index and the chunks, and the at-rest reader, shared
/// by every client (R-CLIENT-1): `krowk_client::sync::store` declares them.
pub use krowk_client::sync::store::{attach, catch_up, open_index, open_session_key, Attached, Checkpoint, Chunk, Head, HostedOn, Index, Mark};

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

    /// The ids of the events the registry's log holds.
    pub fn stored_ids(&self) -> std::collections::HashSet<String> {
        self.events.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect()
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

