//! Declare, upload, finalize, expire, claim and take down — ported from the Go
//! stand-in's registry_test.go.

mod common;

use common::*;
use jiff::SignedDuration;
use gruntas_devregistry::{EPHEMERAL_LIFETIME, UPLOAD_URL_LIFETIME};
use serde_json::{Value, json};

const MINUTE: SignedDuration = SignedDuration::from_mins(1);

fn testdata(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/testdata/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Declares, uploads with the handed-back headers, and finalizes.
fn measured(s: &Server, name: &str, content_type: &str, body: &[u8]) -> Value {
    let payload = declare_typed(s, TEST_KEY, name, content_type, body.len());
    assert!(payload["width"].is_null(), "measured before storage confirmed the bytes");
    assert_eq!(put_signed(&payload, body), 200);
    let r = finalize(s, TEST_KEY, &payload);
    assert_eq!(r.status, 200, "finalize = {}", r.text());
    r.json()
}

/// The registry measures an image at finalize and serves the pair on every read.
#[test]
fn finalize_measures_an_image_and_serves_its_size() {
    let s = Server::new();
    let ready = measured(&s, "shot.png", "image/png", &png_bytes(320, 200));
    assert_eq!((ready["width"].as_i64(), ready["height"].as_i64()), (Some(320), Some(200)));
}

/// Real files, one per encoding a WebP can hold, all 40x24.
#[test]
fn finalize_measures_every_webp_encoding() {
    for name in ["tiny-vp8.webp", "tiny-vp8l.webp", "tiny-vp8x.webp"] {
        let s = Server::new();
        let ready = measured(&s, name, "image/webp", &testdata(name));
        assert_eq!((ready["width"].as_i64(), ready["height"].as_i64()), (Some(40), Some(24)), "{name}");
    }
}

/// A percentage-sized SVG is deliberately no measurement: it means "however big
/// the box is", which nothing can reserve space from.
#[test]
fn finalize_measures_an_svg_only_when_it_states_a_fixed_size() {
    for (attrs, want) in [
        (r#"width="120px" height="80px""#, Some((120, 80))),
        (r#"width="120pt" height="80pt""#, Some((120, 80))),
        (r#"width="120" height="80""#, Some((120, 80))),
        (r#"width="100%" height="100%" viewBox="0 0 120 80""#, None),
        (r#"viewBox="0 0 120 80""#, None),
    ] {
        let s = Server::new();
        let body = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" {attrs}></svg>"#);
        let ready = measured(&s, "chart.svg", "image/svg+xml", body.as_bytes());
        let got = ready["width"].as_i64().zip(ready["height"].as_i64());
        assert_eq!(got, want, "{attrs}");
    }
}

/// Nothing to measure is null — present, and not a zero.
#[test]
fn an_artifact_with_nothing_to_measure_sends_null_dimensions() {
    let s = Server::new();
    for (name, ct, body) in [("notes.txt", "text/plain", "the bytes"), ("shot.png", "image/png", "not an image")] {
        let ready = measured(&s, name, ct, body.as_bytes());
        assert_eq!(ready["state"], "ready", "a measurement must never fail a push");
        let o = ready.as_object().unwrap();
        assert!(o.contains_key("width") && ready["width"].is_null() && ready["height"].is_null(), "{ready}");
    }
}

/// Past the 24-hour lifetime, every endpoint that can meet the artifact answers
/// 410 — the bytes included, and the card says so rather than 404.
#[test]
fn ephemeral_expiry_answers_gone_everywhere() {
    let s = Server::new();
    let payload = declare(&s, "", "a.txt", "the bytes");
    assert_eq!(put(&upload_url(&payload), "the bytes"), 200);
    let (slug, token) = (str_of(&payload, "slug"), str_of(&payload, "claim_token"));

    s.advance(EPHEMERAL_LIFETIME + MINUTE);

    let r = show(&s, "", slug);
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
    let r = finalize(&s, "", &payload);
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
    let r = claim(&s, TEST_KEY, slug, token);
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
    let r = presign(&s, "", slug, token);
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
    assert_eq!(request("GET", str_of(&payload, "file_url"), "", "", "").status, 404);
    assert_eq!(request("GET", str_of(&payload, "url"), "", "", "").status, 410);
}

#[test]
fn a_claim_just_before_expiry_still_succeeds_and_a_paid_one_keeps() {
    let s = Server::new();
    let payload = declare(&s, "", "a.txt", "the bytes");
    let slug = str_of(&payload, "slug");
    s.advance(EPHEMERAL_LIFETIME - MINUTE);
    assert_eq!(claim(&s, TEST_KEY, slug, str_of(&payload, "claim_token")).status, 200);
    s.advance(SignedDuration::from_hours(48));
    assert_eq!(show(&s, TEST_KEY, slug).status, 200, "claimed artifact expired anyway");
}

/// Claiming into a free workspace restamps the expiry rather than lifting it:
/// a fresh day, and no longer.
#[test]
fn a_claim_into_a_free_workspace_restamps_the_expiry() {
    let s = Server::new();
    let payload = declare(&s, "", "a.txt", "the bytes");
    let slug = str_of(&payload, "slug");
    s.advance(EPHEMERAL_LIFETIME - MINUTE);
    let r = claim(&s, "krowk_sk_free", slug, str_of(&payload, "claim_token"));
    assert_eq!(r.status, 200);
    assert!(!r.json()["expires_at"].is_null(), "claim into free lifted the expiry");
    s.advance(SignedDuration::from_hours(1));
    assert_eq!(show(&s, "krowk_sk_free", slug).status, 200);
    s.advance(EPHEMERAL_LIFETIME);
    assert_eq!(show(&s, "krowk_sk_free", slug).status, 410);
}

/// A keyed upload expires on a free plan, with no claim token, and never on a
/// paid one.
#[test]
fn a_keyed_upload_expires_only_on_a_free_plan() {
    let s = Server::new();
    let free = declare(&s, "krowk_sk_free", "a.txt", "the bytes");
    assert!(!free["expires_at"].is_null() && free.get("claim_token").is_none(), "{free}");
    let paid = declare(&s, TEST_KEY, "a.txt", "the bytes");
    assert!(paid["expires_at"].is_null(), "{paid}");
    s.advance(EPHEMERAL_LIFETIME + MINUTE);
    assert_eq!(show(&s, "krowk_sk_free", str_of(&free, "slug")).status, 410);
    assert_eq!(show(&s, TEST_KEY, str_of(&paid, "slug")).status, 200);
}

/// The upload URL's 15 minutes are enforced, and a represign recovers from the
/// lapse without moving anything the link depends on.
#[test]
fn a_represign_recovers_a_lapsed_upload() {
    let s = Server::new();
    let declared = declare(&s, TEST_KEY, "a.txt", "the bytes");
    let slug = str_of(&declared, "slug");
    s.advance(UPLOAD_URL_LIFETIME + MINUTE);
    let late = request("PUT", &upload_url(&declared), "", "text/plain", "the bytes");
    assert_eq!(late.status, 403);
    assert!(late.text().contains("<Code>AccessDenied</Code>"), "{}", late.text());

    let r = presign(&s, TEST_KEY, slug, "");
    assert_eq!(r.status, 200, "{}", r.text());
    let fresh = r.json();
    for k in ["slug", "url", "byte_size"] {
        assert_eq!(fresh[k], declared[k], "represign moved {k}");
    }
    assert_ne!(upload_url(&fresh), upload_url(&declared));
    assert_eq!(put(&upload_url(&fresh), "the bytes"), 200);
    assert_eq!(finalize(&s, TEST_KEY, &declared).status, 200);
}

/// The slug reads a keyless artifact; only the claim token re-mints its PUT.
#[test]
fn represigning_a_keyless_artifact_needs_its_claim_token() {
    let s = Server::new();
    let declared = declare(&s, "", "a.txt", "the bytes");
    let (slug, token) = (str_of(&declared, "slug"), str_of(&declared, "claim_token"));
    let r = presign(&s, "", slug, "");
    assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"));
    let r = presign(&s, "", slug, "krowk_claim_garbage");
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    assert_eq!(presign(&s, TEST_KEY, slug, "").status, 404, "an unrelated key is no authority");

    let r = presign(&s, "", slug, token);
    assert_eq!(r.status, 200);
    let fresh = r.json();
    assert_ne!(upload_url(&fresh), upload_url(&declared));
    assert!(fresh.get("claim_token").is_none(), "the represign re-issued the claim token");
}

#[test]
fn represigning_a_finalized_artifact_is_refused() {
    let s = Server::new();
    let declared = ready_artifact(&s, TEST_KEY, "the bytes");
    let r = presign(&s, TEST_KEY, str_of(&declared, "slug"), "");
    assert_eq!((r.status, r.code().as_str()), (409, "already_finalized"));
}

/// A real presigned URL signs the key: bytes land exactly where the artifact
/// says they live.
#[test]
fn the_upload_token_is_bound_to_the_key() {
    let s = Server::new();
    let payload = declare(&s, "", "f.txt", "the bytes");
    assert_eq!(put(&upload_url(&payload).replacen("/f.txt", "/other.png", 1), "the bytes"), 403);
    assert_eq!(finalize(&s, "", &payload).status, 409);
    assert_eq!(put(&upload_url(&payload), "the bytes"), 200);
}

/// Storage refuses a PUT without the checksum header it handed out, the length
/// it was declared at, or the digest.
#[test]
fn an_upload_is_held_to_its_headers_length_and_digest() {
    let s = Server::new();
    let body = "the bytes";
    let sum = "2a3d5f42bfa23d6a5c5ea2e2a1f2b91e37e4fc2d1f0d4d0ee26e3a7d50fb93a2";
    let declare_sum = |checksum: &str| {
        let d = json!({"artifact": {"filename": "a.txt", "content_type": "text/plain", "byte_size": body.len(), "checksum": checksum}});
        let r = request("POST", &s.at("/v1/artifacts"), "", "application/json", &d.to_string());
        assert_eq!(r.status, 201);
        r.json()
    };
    let right = sha256(body);
    let payload = declare_sum(&right);
    let header = payload["upload"]["headers"]["x-amz-checksum-sha256"].as_str().unwrap().to_owned();
    assert_eq!(header.len(), 44, "{header}");
    let url = upload_url(&payload);
    let r = send("PUT", &url, &[("Content-Type", "text/plain")], body.as_bytes());
    assert_eq!((r.status, r.header("Content-Type")), (403, Some("application/xml")));
    let signed = [("Content-Type", "text/plain"), ("x-amz-checksum-sha256", header.as_str())];
    let r = send("PUT", &url, &signed, b"the byte");
    assert!(r.status == 400 && r.text().contains("IncorrectContentLength"), "{}", r.text());
    assert_eq!(send("PUT", &url, &signed, body.as_bytes()).status, 200);

    // A digest that matches no bytes is refused at the edge.
    let wrong = declare_sum(sum);
    let header = wrong["upload"]["headers"]["x-amz-checksum-sha256"].as_str().unwrap().to_owned();
    let r = send("PUT", &upload_url(&wrong), &[("Content-Type", "text/plain"), ("x-amz-checksum-sha256", &header)], body.as_bytes());
    assert!(r.status == 400 && r.text().contains("BadDigest"), "{}", r.text());
}

fn sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s).iter().map(|b| format!("{b:02x}")).collect()
}

/// Finalizing is idempotent: the second is the same success, not a transition.
#[test]
fn finalizing_twice_returns_the_same_artifact() {
    let s = Server::new();
    let payload = declare(&s, TEST_KEY, "shot.txt", "the bytes");
    assert_eq!(put(&upload_url(&payload), "the bytes"), 200);
    let first = finalize(&s, TEST_KEY, &payload).json();
    let second = finalize(&s, TEST_KEY, &payload).json();
    assert_eq!(first, second);
    assert_eq!(first["state"], "ready");
}
