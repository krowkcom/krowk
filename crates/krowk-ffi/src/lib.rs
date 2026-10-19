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
//!    KrowkError>`.
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
//!
//! UniFFI cannot export a `const`, so a constant becomes a function
//! ([`protocol_version`]); nor a `FromStr` or `Display` impl, so those
//! become `parse_*` and `*_to_string` functions ([`parse_pairing_code`]).
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
//! `make check`.

uniffi::setup_scaffolding!();

mod error;

pub use error::KrowkError;

use krowk_client::pairing::PairingCode;

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
