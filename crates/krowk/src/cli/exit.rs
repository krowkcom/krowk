//! Exit codes are a contract a script branches on, so every failure maps to
//! one here and nowhere else. The table is in `krowk help`.

use krowk_api::Error;

pub const OK: i32 = 0;
pub const USAGE: i32 = 1;
pub const NOT_FOUND: i32 = 2;
pub const AUTH: i32 = 3;
pub const REFUSED: i32 = 4;
pub const RATE_LIMITED: i32 = 5;
pub const UNREACHABLE: i32 = 6;
pub const SERVER: i32 = 7;
pub const GONE: i32 = 8;

/// Codes this client raises itself, whatever status rode along.
fn client(code: &str) -> Option<i32> {
    Some(match code {
        "network_unreachable" | "storage_unreachable" | "storage_rejected_upload" | "import_locked" | "store_unavailable" => {
            UNREACHABLE
        }
        "unsupported_os" | "import_failed" | "confirmation_required" | "selection_cancelled" | "ambiguous_session" => USAGE,
        "no_session" => NOT_FOUND,
        "malformed_response" | "refused_verification_url" => SERVER,
        "not_authenticated" | "missing_claim" | "no_key_for_workspace" | "dangling_default" | "authorization_denied"
        | "no_one_to_approve" => AUTH,
        "private_needs_key" | "shared_needs_key" | "visibility_not_applied" | "budget_exceeded" => REFUSED,
        // A backend is not started in a repository nobody trusted (R-BACK-6).
        #[cfg(feature = "harness")]
        "untrusted_directory" => REFUSED,
        // `krowk status` with no instance ready: nothing is signed in or
        // keyed, the same class as a missing login.
        #[cfg(feature = "harness")]
        "none_ready" => AUTH,
        "authorization_expired" => GONE,
        _ => return None,
    })
}

/// Codes the registry answers with, honoured only on an answer that came back.
fn registry(code: &str) -> Option<i32> {
    Some(match code {
        "not_found" | "no_such_endpoint" => NOT_FOUND,
        "unauthorized" => AUTH,
        "already_finalized" | "idempotency_key_reused" | "upload_missing" | "run_needs_key" | "checksum_mismatch"
        | "empty_upload" | "invalid" | "parameter_missing" | "bad_request" => REFUSED,
        "too_many_requests" => RATE_LIMITED,
        "storage_unavailable" => UNREACHABLE,
        "expired" | "taken_down" | "spent" => GONE,
        "internal_server_error" => SERVER,
        _ => return None,
    })
}

pub fn code_for(err: &Error) -> i32 {
    let code = err.code();
    if let Some(exit) = client(&code) {
        return exit;
    }
    if let Some(exit) = registry(&code).filter(|_| err.status != 0) {
        return exit;
    }
    match err.status {
        0 => USAGE,
        401 | 403 => AUTH,
        404 => NOT_FOUND,
        410 => GONE,
        429 => RATE_LIMITED,
        s if s >= 500 => SERVER,
        s if s >= 400 => REFUSED,
        _ => USAGE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krowk_api::fail;

    #[test]
    fn a_code_this_client_raised_wins_and_a_status_decides_the_rest() {
        assert_eq!(code_for(&fail("not_authenticated", "")), AUTH);
        assert_eq!(code_for(&fail("bad_flag", "")), USAGE);
        let answered = |code: &str, status| Error { status, ..fail(code, "") };
        assert_eq!(code_for(&answered("taken_down", 410)), GONE);
        assert_eq!(code_for(&answered("whatever", 503)), SERVER);
        assert_eq!(code_for(&answered("whatever", 422)), REFUSED);
        // A registry code means nothing on a failure that never reached it.
        assert_eq!(code_for(&fail("not_found", "")), USAGE);
    }
}
