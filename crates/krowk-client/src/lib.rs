//! What every krowk client shares (R-CLIENT-1): the terminal links it
//! today, the desktop app (gpui) and the phones (UniFFI) later, so the
//! end-to-end encryption is written once and never again per platform.
//!
//! - `e2e`: the constructions — device keypairs, the account key wrapped to
//!   each device with HPKE, session keys wrapped under the account key, and
//!   session content sealed under a session key (R-E2E-2, R-E2E-3).
//! - `phrase`: the account key as 24 words, shown once at first sync setup
//!   and the only way back to it on a fresh machine (R-E2E-4).
//! - `keystore`: the device key and the wrapped account key in krowk's
//!   home, `0600`.
//! - `protocol`: the typed commands, events and log lines every client
//!   speaks, and the daemon's frame-envelope layout (`protocol::frame`).
//!
//! The design, and what a hostile server can and cannot learn from it, is
//! `engineering/crypto.md` in Canon (R-OSS-1).

pub mod e2e;
pub mod keystore;
pub mod phrase;
pub mod protocol;
pub mod relay_ticket;

/// A secret the caller holds for a moment — a typed phrase — wiped on drop.
pub use zeroize::Zeroizing;
