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
    /// JSON handed in is not the protocol type the call reads: malformed,
    /// or a `type` this build does not know (a newer daemon's).
    #[error("{detail}")]
    Decode { detail: String },
}

/// A Swift `KeyStorage` that threw something other than a `KrowkError`,
/// or whose call UniFFI could not complete.
impl From<uniffi::UnexpectedUniFFICallbackError> for KrowkError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> KrowkError {
        KrowkError::Internal { detail: format!("the app's key storage failed: {}", e.reason) }
    }
}

/// A registry call's failure: no answer (from the registry or the object
/// storage it serves chunks from), a rate limit or a 5xx is `Network`; any
/// other refusal is `Refused` with the registry's code.
impl From<krowk_api::Error> for KrowkError {
    fn from(e: krowk_api::Error) -> KrowkError {
        let code = e.code();
        // The terminal's advice names KROWK_API_URL, which a phone has not.
        // Storage is reached through the registry's links: to the person,
        // the same service out of reach.
        if code == "network_unreachable" || code == "storage_unreachable" {
            return KrowkError::Network { detail: "krowk's registry cannot be reached — check the network and try again".into() };
        }
        // The registry's words send the person to `krowk upgrade`, a
        // terminal's command; on a phone it is the app that is too old.
        if code == "signature_version_retired" {
            return KrowkError::Refused { code, detail: "this version of the app is too old for krowk's registry — update the app and try again".into() };
        }
        let detail = [e.fix(), message(&e)].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| code.clone());
        let detail = detail.replace("KROWK_API_URL", "the app's registry address");
        if e.status == 429 || e.status >= 500 {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticket 168: the registry's refusal of v1 signing reaches the app
    /// typed, in the app's words rather than the terminal's.
    #[test]
    fn a_retired_signature_version_is_a_typed_refusal_to_update_the_app() {
        let mut e = krowk_api::fail("signature_version_retired", "this krowk is older than 0.42.0 and signs registry requests with the retired v1 lines — run `krowk upgrade`");
        e.status = 401;
        match KrowkError::from(e) {
            KrowkError::Refused { code, detail } => {
                assert_eq!(code, "signature_version_retired");
                assert!(detail.contains("update the app") && !detail.contains("krowk upgrade"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }
}
