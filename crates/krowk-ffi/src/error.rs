//! The crate's one error type, as Swift sees it (rule 7).

/// Every error an export returns; `detail` says what went wrong.
// Not in the doc comment, which UniFFI copies into the Swift: struct
// variants, each with a `detail` field and never `message`, since Swift's
// `Error` (and Kotlin's `Throwable.message`) already has one and the
// generated code clashes. `From` impls map krowk's own errors, so an export
// maps with `?` rather than by hand at each call.
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
    /// The registry, the person or the app's own state said no, for a
    /// reason `code` names (`authorization_denied`, `signed_out`, …) and
    /// the app may act on.
    #[error("{detail}")]
    Refused { code: String, detail: String },
    /// The registry could not be reached, or kept failing, for as long as
    /// the call was willing to try.
    #[error("{detail}")]
    Network { detail: String },
}

/// A Swift `KeyStorage` that threw something other than a `KrowkError`,
/// or whose call UniFFI could not complete.
impl From<uniffi::UnexpectedUniFFICallbackError> for KrowkError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> KrowkError {
        KrowkError::Internal { detail: format!("the app's key storage failed: {}", e.reason) }
    }
}

/// A registry call's failure: no answer, a rate limit or a 5xx is
/// `Network`; any other refusal is `Refused` with the registry's code.
impl From<krowk_api::Error> for KrowkError {
    fn from(e: krowk_api::Error) -> KrowkError {
        let code = e.code();
        let detail = [e.fix(), message(&e)].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| code.clone());
        if code == "network_unreachable" || e.status == 429 || e.status >= 500 {
            KrowkError::Network { detail }
        } else {
            KrowkError::Refused { code, detail }
        }
    }
}

fn message(e: &krowk_api::Error) -> String {
    match e.body.get("message") {
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}
