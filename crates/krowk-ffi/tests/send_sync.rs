//! R-FFI-1: every object krowk-ffi exports is `Send + Sync + 'static`,
//! since Swift holds it in any thread and UniFFI shares it behind an `Arc`.
//! Checked at compile time: this file stops compiling when one is not.

/// Compiles only for a type every thread may hold and share.
fn assert_send_sync<T: Send + Sync + 'static>() {}

#[test]
fn r_ffi_1_exported_types_are_send_and_sync() {
    // Each `uniffi::Object` is added here, beside the error every export
    // returns and the record sign-in gives back.
    assert_send_sync::<krowk_ffi::KrowkError>();
    assert_send_sync::<krowk_ffi::SignedIn>();
    assert_send_sync::<krowk_ffi::SignIn>();
    assert_send_sync::<krowk_ffi::Account>();
    assert_send_sync::<krowk_ffi::OpenedSession>();
    assert_send_sync::<krowk_ffi::SessionList>();
    // The live viewer and the chunks it read, which Swift reads off the
    // main thread and hands back on the viewer's queue.
    assert_send_sync::<krowk_ffi::Viewer>();
    assert_send_sync::<krowk_ffi::ChunksRead>();
    assert_send_sync::<krowk_ffi::Effect>();
    // The app's storage, which every thread may call.
    assert_send_sync::<std::sync::Arc<dyn krowk_ffi::KeyStorage>>();
}
