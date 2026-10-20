//! A synced session at rest (engineering/harness.md → Sync): the shapes of
//! its sealed index and of the chunks of its log, checkpoints among them.
//! Reading and writing them is the harness's (`krowk_harness::sync::store`).
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
}
