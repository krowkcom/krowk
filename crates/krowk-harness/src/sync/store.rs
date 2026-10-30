//! A synced session at rest (engineering/harness.md → Sync): the lease
//! holder's writer, which seals and stores the chunks of its log and the
//! sealed index. The shapes of the index and the chunks, and reading them
//! back (`attach`, `catch_up`, and the key and index openers), are every
//! client's (R-CLIENT-1): `krowk_client::sync::store` declares them, and
//! they are re-exported here so the harness's paths do not move.

use krowk_api::Client;
use krowk_client::e2e::{self, ChunkSealer, SessionKeys};
use krowk_client::sync::store::read_all;
use crate::protocol::ImageRef;
use serde_json::Value;

/// The shapes of the index and the chunks, and the at-rest reader, shared
/// by every client (R-CLIENT-1): `krowk_client::sync::store` declares them.
pub use krowk_client::sync::store::{attach, catch_up, open_index, open_session_key, resume, Attached, Checkpoint, Chunk, Head, HostedOn, Index, Mark, ReadError, ReadPlace};

fn api(e: krowk_api::Error) -> String {
    e.to_string()
}

/// R-IMG-11: the image `r` names, fetched from session `id`'s image objects
/// and kept in `session_dir`'s `images/` under its sha256, as the host
/// keeps it — a device's own copy, which the next host after a move reads
/// as its own. True once the file is there (already, or now); false while
/// the registry holds no object for it, or the log names it by an older
/// name with no hash. Blocking.
pub fn fetch_image(client: &Client, keys: &SessionKeys, id: &str, session_dir: &std::path::Path, r: &ImageRef) -> Result<bool, String> {
    let Some(sha) = r.sha256.as_deref() else { return Ok(false) };
    if crate::images::path(session_dir, r).is_some_and(|p| p.is_file()) {
        return Ok(true);
    }
    let Some(bytes) = krowk_client::sync::store::fetch_image(client, keys, id, sha).map_err(String::from)? else { return Ok(false) };
    let media_type = crate::images::sniff(&bytes).ok_or("an image object opened to bytes that are no image krowk reads")?;
    crate::images::save(session_dir, &[crate::images::Decoded { number: r.number, media_type, bytes: bytes.into() }]).map_err(|e| e.message)?;
    Ok(true)
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
    /// The session's directory on this machine, whose `images/` the events
    /// name: set by the host (`with_images`), which puts each image as a
    /// sealed object before the chunk naming it.
    dir: Option<std::path::PathBuf>,
    /// The sha256s this writer has put, so each goes once.
    put: std::collections::HashSet<String>,
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
        Ok(Writer { client, key, id: id.to_string(), session, wrapped, sealer, fence, index, events, pending: Vec::new(), staged: None, index_owed: false, dir: None, put: Default::default() })
    }

    /// Puts the images the events name from `session_dir`'s `images/`.
    pub fn with_images(self, session_dir: std::path::PathBuf) -> Writer {
        Writer { dir: Some(session_dir), ..self }
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
            self.put_images(token)?;
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

    /// R-IMG-9: each image the waiting events name, put as a sealed image
    /// object before the chunk naming it is sealed, so a reader of the log
    /// finds every image it names. Once per image: this writer remembers
    /// what it put, and the registry takes a name it holds as a no-op. An
    /// image this machine has no file of (an older log's, named by number)
    /// is passed over: a reader shows its placeholder.
    fn put_images(&mut self, token: &str) -> Result<(), String> {
        let Some(dir) = self.dir.clone() else { return Ok(()) };
        let refs: Vec<ImageRef> = self.pending.iter().filter_map(|e| e.pointer("/item/images")).filter_map(|i| serde_json::from_value::<Vec<ImageRef>>(i.clone()).ok()).flatten().collect();
        for r in refs {
            let Some(sha) = r.sha256.clone().filter(|s| !self.put.contains(s)) else { continue };
            // One kept before the cap was 2.75 MiB may be too large for an
            // object: passed over too, rather than holding the log up.
            let bytes = match crate::images::path(&dir, &r).map(std::fs::read) {
                Some(Ok(b)) if crate::images::sha256(&b) == sha && b.len() + e2e::IMAGE_OVERHEAD <= krowk_api::sync::MAX_IMAGE_OBJECT_BYTES as usize => b,
                _ => continue,
            };
            let hash: [u8; 32] = e2e::unhex(&sha).and_then(|h| h.try_into().ok()).ok_or("an image's sha256 is not 64 hex digits")?;
            let sealed = e2e::seal_image(&self.key, &self.session, &hash, &bytes);
            self.client.put_image_object(&self.id, &e2e::image_name(&self.key, &hash), &sealed, token).map_err(api)?;
            self.put.insert(sha);
        }
        Ok(())
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

