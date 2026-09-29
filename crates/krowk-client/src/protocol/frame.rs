//! The daemon's frame envelope: the 28-byte header every WebSocket message
//! of the protocol starts with, and the values its bytes take. Declared
//! here rather than in the daemon so a client that links no engine — the
//! desktop app, a phone — reads and writes the same header the daemon and
//! the relay do (R-CLIENT-1). The layout, byte by byte, is
//! `krowk_harness::daemon::ws`'s module doc; `Envelope`, which compresses,
//! stays there.
//!
//! `v` (offset 0), `kind` (1), `flags` (2), `enc` (3), `session` (4, 16
//! bytes), `seq` (20, 8 bytes big-endian), then the payload.

/// The header's length: `enc` is its byte 3.
pub const HEADER: usize = 28;
pub const V: u8 = 1;
pub const KIND_BATCH: u8 = 1;
pub const KIND_FRAME: u8 = 2;
pub const KIND_ACK: u8 = 3;
/// A relay's own control message, JSON, never sealed (engineering/relay.md).
pub const KIND_RELAY: u8 = 4;
/// A sealed envelope the relay carries between the host and one viewer,
/// whose link the header's `seq` names (engineering/relay.md).
pub const KIND_ROUTED: u8 = 5;
pub const FLAG_ZSTD: u8 = 1;
pub const ENC_NONE: u8 = 0;
/// The `enc` byte for a payload sealed with `e2e::Sealer::seal`:
/// XChaCha20-Poly1305 under the session key, a random 24-byte nonce first,
/// the 28-byte header as associated data. 0 stays "none". Frames start
/// carrying it with the relay (tickets 17 and 19).
pub const ENC_XCHACHA20_POLY1305: u8 = 1;
