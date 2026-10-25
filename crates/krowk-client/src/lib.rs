//! What every krowk client shares (R-CLIENT-1): the terminal links it
//! today, the desktop app (gpui) and the phones (UniFFI) later, so the
//! end-to-end encryption is written once and never again per platform.
//!
//! - `e2e`: the constructions — device keypairs, the account key wrapped to
//!   each device with HPKE, session keys wrapped under the account key, and
//!   session content sealed under a session key (R-E2E-2, R-E2E-3).
//! - `keystore`: the device key and the wrapped account key in krowk's
//!   home, `0600`.
//! - `pairing`: adding a device by a short code — SPAKE2 with key
//!   confirmation both ways, as a sans-IO state machine for each side.
//! - `pairing_payload`: what the paired device seals to the new one — the
//!   chain, the wrapped user key, its signing key — and the new device's
//!   checks that the chain adds exactly it.
//! - `pairing_join`: the new device's side of a pairing, from the hello to
//!   the claim, over a `Mailbox` each platform supplies.
//! - `user_key`: the person's user key, by generation, HPKE-wrapped to each
//!   device, each generation wrapping the one before it.
//! - `recovery`: the 12-word recovery kit, its words, and the recovery
//!   device it derives.
//! - `device_chain`: the signed, append-only device list and its verifier.
//! - `session_record`: a synced session's record, signed by the device
//!   that published it and checked against the device list.
//! - `presence`: a host's announcement of itself — its instances, models,
//!   directories and session ids — sealed to the person's devices.
//! - `host_command`: a command one device sends a host (`start`), and the
//!   host's answer, each sealed and bound to its id and both devices.
//! - `protocol`: the typed commands, events and log lines every client
//!   speaks, and the daemon's frame-envelope layout (`protocol::frame`).
//! - `sync`: the wire and at-rest shapes of a synced session — a viewer's
//!   frames, the host's answers, welcome and batches, and (`sync::store`)
//!   the sealed index and chunks its log is kept in, with the blocking
//!   reader that opens them from the registry (`attach`, `catch_up`).
//!
//! The design, and what a hostile server can and cannot learn from it, is
//! `docs/security/crypto.md` (R-OSS-1).

pub mod e2e;
pub mod host_command;
pub mod keystore;
pub mod pairing;
pub mod pairing_join;
pub mod pairing_payload;
pub mod presence;
pub mod protocol;
pub mod relay_link;
pub mod relay_ticket;
pub mod device_chain;
pub mod recovery;
pub mod session_record;
pub mod sync;
pub mod user_key;

/// A secret the caller holds for a moment — the kit's words — wiped on drop.
pub use zeroize::Zeroizing;
