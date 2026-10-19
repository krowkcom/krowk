//! R-FFI-1: every object krowk-ffi exports is `Send + Sync + 'static`,
//! since Swift holds it in any thread and UniFFI shares it behind an `Arc`.
//! Checked at compile time: this file stops compiling when one is not.

/// Compiles only for a type every thread may hold and share.
fn assert_send_sync<T: Send + Sync + 'static>() {}

#[test]
fn r_ffi_1_exported_types_are_send_and_sync() {
    // No `uniffi::Object` is exported yet: each one later is added here,
    // beside the error every export returns.
    assert_send_sync::<krowk_ffi::KrowkError>();
}
