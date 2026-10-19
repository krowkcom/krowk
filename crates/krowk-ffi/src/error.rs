//! The crate's one error type, as Swift sees it (rule 7).

/// Every error an export returns; `detail` says what went wrong.
// Not in the doc comment, which UniFFI copies into the Swift: struct
// variants, each with a `detail` field and never `message`, since Swift's
// `Error` (and Kotlin's `Throwable.message`) already has one and the
// generated code clashes. Later variants (`Network`, `Refused { code, detail }`, …) come with `From`
// impls from krowk-client's errors, so an export maps with `?` rather than
// by hand at each call.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum KrowkError {
    /// What the caller passed is not what the call takes; `detail` says
    /// what was expected, for the person.
    #[error("{detail}")]
    InvalidInput { detail: String },
    /// Something failed inside krowk that the caller could not have
    /// prevented.
    #[error("{detail}")]
    Internal { detail: String },
}
