//! Claims and takedowns: who holds the authority over an artifact, and the
//! tombstone a takedown leaves behind.

mod common;

use common::*;
use jiff::SignedDuration;
use gruntas_devregistry::EPHEMERAL_LIFETIME;

/// A claim answers 200 only to the artifact's own token — a garbage one must
/// not ride the retry-after-success affordance.
#[test]
fn a_claim_requires_the_real_token() {
    let s = Server::new();
    let owned = declare(&s, TEST_KEY, "a.txt", "the bytes");
    assert_eq!(claim(&s, TEST_KEY, str_of(&owned, "slug"), "krowk_claim_garbage").status, 404, "a keyed artifact is never claimable");

    let anonymous = declare(&s, "", "b.txt", "the bytes");
    let (slug, token) = (str_of(&anonymous, "slug"), str_of(&anonymous, "claim_token"));
    assert_eq!(claim(&s, TEST_KEY, slug, token).status, 200);
    assert_eq!(claim(&s, TEST_KEY, slug, token).status, 200, "a retry with the real token");
    assert_eq!(claim(&s, TEST_KEY, slug, "krowk_claim_garbage").status, 404);
    assert_eq!(claim(&s, "krowk_sk_other", slug, token).status, 404, "the token is spent for everyone else");
}

/// The bytes stay where the presigned URL was signed for, so a URL issued
/// before the claim keeps working.
#[test]
fn a_claim_does_not_invalidate_the_upload_url() {
    let s = Server::new();
    let payload = declare(&s, "", "a.txt", "the bytes");
    assert_eq!(claim(&s, TEST_KEY, str_of(&payload, "slug"), str_of(&payload, "claim_token")).status, 200);
    assert_eq!(put(&upload_url(&payload), "the bytes"), 200);
    assert_eq!(finalize(&s, TEST_KEY, &payload).status, 200);
}

/// The bytes go at once and for good; what stays is a tombstone answering 410
/// on every endpoint, naming nothing, and in no listing.
#[test]
fn a_takedown_removes_the_bytes_and_answers_gone_everywhere() {
    let s = Server::new();
    let payload = ready_artifact(&s, TEST_KEY, "a secret");
    let slug = str_of(&payload, "slug");
    assert_eq!(take_down(&s, TEST_KEY, slug, "").status, 204);

    assert_eq!(request("GET", str_of(&payload, "file_url"), "", "", "").status, 404);
    let card = request("GET", str_of(&payload, "url"), "", "", "");
    assert_eq!(card.status, 410);
    assert!(!card.text().contains("a.txt"), "the taken-down card names the file:\n{}", card.text());
    let r = show(&s, TEST_KEY, slug);
    assert_eq!((r.status, r.code().as_str()), (410, "taken_down"));
    let r = finalize(&s, TEST_KEY, &payload);
    assert_eq!((r.status, r.code().as_str()), (410, "taken_down"));
    let run = open_run(&s, TEST_KEY);
    let r = attach(&s, TEST_KEY, slug, &run);
    assert_eq!((r.status, r.code().as_str()), (410, "taken_down"));
    let listing = request("GET", &s.at("/v1/artifacts"), TEST_KEY, "", "").json();
    assert!(slugs_of(&listing, "artifacts").is_empty(), "the listing still holds the tombstone");
    assert_eq!(take_down(&s, TEST_KEY, slug, "").status, 204, "a second takedown is the same success");
}

/// A tombstone says it was taken down and nothing else: no time, and an empty
/// details that is present rather than forgotten.
#[test]
fn a_tombstone_names_no_time_and_carries_an_empty_details() {
    let s = Server::new();
    let slug = str_of(&ready_artifact(&s, TEST_KEY, "bytes"), "slug").to_owned();
    assert_eq!(take_down(&s, TEST_KEY, &slug, "").status, 204);
    let e = show(&s, TEST_KEY, &slug).json()["error"].clone();
    assert_eq!(e["message"], format!("{slug} was taken down"));
    assert_eq!(e["details"], serde_json::json!({}));
}

/// For a keyless caller the claim token is the authority, not the slug.
#[test]
fn a_keyless_takedown_needs_the_claim_token_and_not_just_the_slug() {
    let s = Server::new();
    let payload = ready_artifact(&s, "", "the bytes");
    let (slug, token) = (str_of(&payload, "slug"), str_of(&payload, "claim_token"));
    let r = take_down(&s, "", slug, "");
    assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"));
    let r = take_down(&s, "", slug, "krowk_claim_wrong");
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    assert_eq!(show(&s, "", slug).status, 200, "a refused takedown took it down anyway");
    assert_eq!(take_down(&s, "", slug, token).status, 204);
    assert_eq!(show(&s, "", slug).code(), "taken_down");
}

#[test]
fn a_takedown_is_scoped_to_the_keys_workspace() {
    let s = Server::new();
    let slug = str_of(&ready_artifact(&s, "krowk_sk_mine", "the bytes"), "slug").to_owned();
    assert_eq!(take_down(&s, "krowk_sk_theirs", &slug, "").status, 404);
    assert_eq!(show(&s, "krowk_sk_mine", &slug).status, 200);
}

/// Once claimed, the key that claimed it takes it down; the spent token does not.
#[test]
fn a_spent_claim_token_no_longer_takes_an_artifact_down() {
    let s = Server::new();
    let payload = ready_artifact(&s, "", "the bytes");
    let (slug, token) = (str_of(&payload, "slug"), str_of(&payload, "claim_token"));
    assert_eq!(claim(&s, TEST_KEY, slug, token).status, 200);
    assert_eq!(take_down(&s, "", slug, token).status, 404);
    assert_eq!(take_down(&s, TEST_KEY, slug, "").status, 204);
}

/// A fresh claim on a tombstone is 404, as the registry's `live` scope answers
/// it — while a retry of a claim that already landed is still 200.
#[test]
fn a_taken_down_artifact_cannot_be_claimed_but_a_retry_still_succeeds() {
    let s = Server::new();
    let payload = ready_artifact(&s, "", "the bytes");
    let (slug, token) = (str_of(&payload, "slug"), str_of(&payload, "claim_token"));
    assert_eq!(take_down(&s, "", slug, token).status, 204);
    let r = claim(&s, TEST_KEY, slug, token);
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));

    let payload = ready_artifact(&s, "", "the bytes");
    let (slug, token) = (str_of(&payload, "slug"), str_of(&payload, "claim_token"));
    assert_eq!(claim(&s, TEST_KEY, slug, token).status, 200);
    assert_eq!(take_down(&s, TEST_KEY, slug, "").status, 204);
    let r = claim(&s, TEST_KEY, slug, token);
    assert_eq!((r.status, str_of(&r.json(), "slug")), (200, slug));
}

/// Both at once, the one somebody decided is the truer answer.
#[test]
fn a_takedown_is_reported_ahead_of_an_expiry() {
    let s = Server::new();
    let payload = ready_artifact(&s, "", "the bytes");
    let slug = str_of(&payload, "slug");
    assert_eq!(take_down(&s, "", slug, str_of(&payload, "claim_token")).status, 204);
    s.advance(EPHEMERAL_LIFETIME + SignedDuration::from_mins(1));
    assert_eq!(show(&s, "", slug).code(), "taken_down");
}

/// Declared but not uploaded: the takedown spends the live PUT, so the bytes
/// cannot land under the tombstone.
#[test]
fn a_takedown_spends_an_outstanding_upload_url() {
    let s = Server::new();
    let payload = declare(&s, "", "a.txt", "the bytes");
    assert_eq!(take_down(&s, "", str_of(&payload, "slug"), str_of(&payload, "claim_token")).status, 204);
    assert_eq!(put(&upload_url(&payload), "the bytes"), 403);
    assert_eq!(request("GET", str_of(&payload, "file_url"), "", "", "").status, 404);
}
