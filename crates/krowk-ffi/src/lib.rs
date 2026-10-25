//! krowk-client for the phones, through UniFFI (R-FFI-1, R-CLIENT-1): the
//! iPhone app calls this crate from Swift and never reimplements the
//! crypto, the protocol or sync. Swift sees a module `Krowk` over a C
//! module `KrowkFFI` (`uniffi.toml`).
//!
//! # Rules every export follows
//!
//! 1. Synchronous only. No async runtime in the FFI, no tokio. Swift owns
//!    the sockets (Network.framework); Rust is sans-IO for the phone: Swift
//!    passes bytes in and gets back bytes to send and a state diff.
//! 2. Blocking registry calls (ureq, through krowk-api) are allowed as
//!    plain sync methods; Swift calls them off the main thread.
//! 3. Objects are `#[derive(uniffi::Object)]` held in `Arc`, constructed
//!    with `#[uniffi::constructor] fn open(...) -> Result<Arc<Self>,
//!    KrowkError>`, or returned so by another object's method
//!    (`Account::open_session`).
//! 4. Every exported object is `Send + Sync + 'static`, checked at compile
//!    time by `tests/send_sync.rs`.
//! 5. Protocol and core types are re-declared with
//!    `#[uniffi::remote(Record)]` / `#[uniffi::remote(Enum)]`, so the
//!    compiler checks each copy against the real type; never a
//!    hand-written mirror type (R-PROTO-2). Newtype ids use
//!    `uniffi::custom_type!(Id, String, { remote, lower: |id|
//!    id.as_str().to_owned(), try_lift: |s| Ok(Id::new(s)) })`;
//!    `serde_json::Value` crosses as a JSON string; bytes as `Vec<u8>`;
//!    fields added later get `#[uniffi(default = None)]`.
//! 6. Storage the phone owns (the Keychain) is a foreign trait:
//!    `#[uniffi::export(with_foreign)] pub trait KeyStorage: Send + Sync`.
//! 7. Errors: one [`KrowkError`], struct variants, field `detail`.
//! 8. The public API is frozen in `public-api.txt` (R-FFI-2), rendered
//!    from this source by `tests/public_api.rs`, which fails when the two
//!    differ. A change that adds an export reruns it with
//!    `UPDATE_PUBLIC_API=1` and commits the new file; one that removes or
//!    changes an existing item also bumps [`FFI_API_VERSION`], here and on
//!    the file's first line, or the rewrite is refused. Swift reads the
//!    version at run time through [`ffi_api_version`].
//!
//! UniFFI cannot export a `const`, so a constant becomes a function
//! ([`protocol_version`]); nor a `FromStr` or `Display` impl, so those
//! become `parse_*` and `*_to_string` functions ([`parse_pairing_code`]).
//!
//! # Signing in, and where the keys live
//!
//! The app implements [`KeyStorage`] over the Keychain (R-FFI-3): the
//! keystore's five items and `credentials.json`, the krowk API key, under
//! the file names the terminal uses. [`SignIn`] is the browser sign-in,
//! saving the key there on approval; [`Account`] opens from it, makes the
//! device's keys on first use, and holds the client that signs registry
//! calls as this device. Nothing exported returns a private key.
//!
//! # Joining a pairing
//!
//! A signed-in phone holds no user key until it is paired (R-E2E-3).
//! [`Account::join_pairing`] takes the code another of the person's
//! devices shows (`krowk devices add`), typed — or [`Account::join_invite`]
//! an invite read from its QR code by [`parse_pairing_invite`], checked
//! against the account and the open pairing first — and runs
//! krowk-client's joining side, `pairing_join::join`, over
//! [`RegistryMailbox`]; the user keys and the device list land in the
//! app's storage. Blocking, for up to the ten minutes the other device's
//! person has to approve (rule 2); [`Account::cancel_pairing`] ends it
//! from any thread, until the keys are kept. [`Account::open_pairing`]
//! names the device showing the code before any is spent, and
//! [`Account::devices`] reads the device list the join kept.
//!
//! # Still on the person's devices
//!
//! [`Account::check_device_list`] reads the device list the registry
//! serves and holds it to the one the phone pinned, as `krowk sync status`
//! does: a phone removed (or whose key the registry refuses) is refused
//! `device_removed`, one left off a list that was started over
//! `device_list_reset`, and either stops syncing until it is reset. A list
//! that only grew becomes the pin, and a newer user key generation is
//! taken up. [`Account::has_recovery_kit`] reads whether the pinned list
//! holds a recovery kit, for the reminder after pairing.
//!
//! # Sessions
//!
//! [`Account::list_sessions`] lists the person's synced sessions from their
//! sealed indexes, each record checked against the device list the phone
//! holds; [`Account::open_session`] opens one from its latest checkpoint
//! into an [`OpenedSession`], which reads on with `catch_up` and gets a
//! relay ticket for the live viewer. Listing, opening, `catch_up` and the
//! ticket are blocking registry calls (rule 2); the getters never wait on
//! the network. The session's keys, the device's signing key and the read
//! position stay inside `OpenedSession`; Swift gets events, titles and
//! heads. A head crosses as [`HeadRecord`], a record of its own rather than
//! the remote type rule 5 asks for: `Head`'s digest is a `[u8; 32]`, which
//! UniFFI has no word for, so the record carries it as bytes, checked to be
//! 32 on the way back in.
//!
//! # Following a session live
//!
//! [`Viewer`] opens on an `OpenedSession` and wraps krowk-client's
//! `ViewerCore` and the relay's join handshake behind a mutex. Swift owns
//! the WebSocket and the timers (rule 1): it passes in `start`, the relay
//! ticket (`ticket`, `ticket_failed`), each message received (a
//! [`WsFrame`]), `closed`, `tick` at `next_wake_ms`, the person's
//! `command`s and the chunks `read_chunks` read (`chunks_read`), each with
//! its own clock's `now_ms`, and carries out the [`Effect`]s each returns:
//! dial, send, ping, close, show the [`ViewerUpdate`]s, read the chunks,
//! end. Each dial is numbered and what comes from a connection names it,
//! so a dropped one's late messages are ignored; the join's ten-second
//! waits are `tick`'s too. Every rule of the viewer is Rust's; the
//! session's keys and the signing key stay inside. Relay only: a race for
//! a direct path the core asks for is lost at once, here.
//!
//! # The protocol
//!
//! Every type reachable from `StreamLine` and `Command` is
//! `krowk_client::protocol`'s own, re-declared remote in `src/protocol.rs`;
//! JSON crosses only at the edges, through [`decode_stream_line`],
//! [`decode_log_event`], [`encode_log_event`] and [`encode_command`].
//!
//! # Generating the Swift bindings
//!
//! Library mode reads the interface out of the host build's cdylib (on
//! macOS `libkrowk_ffi.dylib`). `--no-format` because swiftformat is not
//! on Linux:
//!
//! ```sh
//! cargo build -p krowk-ffi --features bindgen --lib --bin uniffi-bindgen
//! target/debug/uniffi-bindgen generate --library target/debug/libkrowk_ffi.so \
//!   --language swift --no-format --out-dir target/krowk-ffi-swift
//! ```
//!
//! That writes `Krowk.swift`, `KrowkFFI.h` and `KrowkFFI.modulemap`;
//! `tests/swift_bindings.rs` generates them the same way on every
//! `make check`. `make ios-core` (`scripts/ios_core.sh`) does too, and
//! wraps them with the iOS static libraries in the XCFramework and Swift
//! package the app links, `apps/ios/KrowkCore`.

uniffi::setup_scaffolding!();

mod account;
mod error;
mod pairing;
mod protocol;
mod sessions;
mod sign_in;
mod standing;
mod storage;
mod supervisor;
mod viewer;

pub use account::{Account, ListedDevice};
pub use error::KrowkError;
pub use pairing::{Joined, OpenPairing, PairingInvite, RegistryMailbox, parse_pairing_invite};
pub use protocol::{decode_log_event, decode_stream_line, encode_command};
pub use protocol::encode_log_event;
pub use sessions::{HeadRecord, OpenedSession, SessionList, SessionSummary};
pub use sign_in::{SignIn, SignedIn};
pub use storage::KeyStorage;
pub use supervisor::{ConnectionSupervisor, LinkState, WakeAction};
pub use viewer::{ChunksRead, Effect, Viewer, ViewerUpdate, WsFrame};

use krowk_client::pairing::PairingCode;

/// The version of this crate's exported API, the first line of
/// `public-api.txt`: bumped whenever an existing export is removed or
/// changed (rule 8), never for an addition. A change to the error an
/// export returns for a case counts too, though the file can't show it:
/// Swift that matches on the old variant would misread it (4: `SignIn.wait`
/// refuses a key the storage won't keep as `credentials_unwritable`; 5:
/// `join_pairing` and `join_invite` take the os the phone runs, and
/// `join_invite`'s `invite_mismatch` is `invite_other_account` or
/// `invite_other_pairing`).
pub const FFI_API_VERSION: u32 = 5;

/// [`FFI_API_VERSION`], for the app to read at run time.
#[uniffi::export]
pub fn ffi_api_version() -> u32 {
    FFI_API_VERSION
}

/// The protocol version this build speaks,
/// `krowk_client::protocol::PROTOCOL_VERSION`.
#[uniffi::export]
pub fn protocol_version() -> u32 {
    krowk_client::protocol::PROTOCOL_VERSION
}

/// A pairing code as typed, in its canonical `XXXX-XXXX` form: case,
/// spaces and dashes are ignored, anything else is refused.
#[uniffi::export]
pub fn parse_pairing_code(text: String) -> Result<String, KrowkError> {
    let code = PairingCode::parse(&text).ok_or_else(|| KrowkError::InvalidInput {
        detail: "that is not a pairing code — it is the eight characters `krowk devices add` shows, like K7QF-9M3X; nothing was sent".into(),
    })?;
    Ok(code.to_string())
}
