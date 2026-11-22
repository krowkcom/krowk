//! A client names its attempt with an Idempotency-Key, and the same key answers
//! with the record it made the first time. What dedupes is the attempt, never
//! the content — ported from the Go stand-in's idempotency_test.go.

mod common;

use common::*;
use jiff::SignedDuration;
use gruntas_devregistry::{EPHEMERAL_LIFETIME, UPLOAD_URL_LIFETIME};
use serde_json::{Value, json};

fn declare_keyed(s: &Server, token: &str, key: Option<&str>, filename: &str) -> Response {
    let body = json!({"artifact": {"filename": filename, "content_type": "text/plain", "byte_size": 9}}).to_string();
    keyed("POST", &s.at("/v1/artifacts"), token, key, &body)
}

fn open_run_keyed(s: &Server, token: &str, key: Option<&str>, commit: &str) -> Response {
    keyed("POST", &s.at("/v1/runs"), token, key, &json!({"run": {"metadata": {"commit": commit}}}).to_string())
}

fn slug(r: &Response) -> String {
    let v: Value = r.json();
    let slug = str_of(&v, "slug");
    assert!(!slug.is_empty(), "no slug in {v}");
    slug.to_owned()
}

#[test]
fn one_key_declares_one_artifact() {
    let s = Server::new();
    let first = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    let second = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    assert_eq!(second.status, 201);
    assert_eq!(slug(&first), slug(&second));
}

/// A signature lasts 15 minutes and a pending artifact far longer, so a replay
/// mints a new one — and the claim token is not re-issued.
#[test]
fn a_replay_is_handed_a_fresh_upload_url() {
    let s = Server::new();
    let first = declare_keyed(&s, "", Some("push-1"), "a.txt").json();
    s.advance(UPLOAD_URL_LIFETIME + SignedDuration::from_nanos(1));
    let second = declare_keyed(&s, "", Some("push-1"), "a.txt").json();
    assert_ne!(upload_url(&first), upload_url(&second));
    assert!(second.get("claim_token").is_none());
    assert_eq!(put(&upload_url(&second), "the bytes"), 200);
}

/// The same key with a different payload is refused, naming what it made.
#[test]
fn a_key_arriving_with_a_different_payload_is_refused() {
    let s = Server::new();
    let first = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    let r = declare_keyed(&s, TEST_KEY, Some("push-1"), "b.txt");
    assert_eq!((r.status, r.code().as_str()), (409, "idempotency_key_reused"));
    assert!(r.json()["error"]["message"].as_str().unwrap().contains(&slug(&first)));
}

/// Identical bytes do not dedupe: without a key nothing does, and a second key
/// is a second artifact.
#[test]
fn only_the_same_key_collapses_a_create() {
    let s = Server::new();
    let (a, b) = (declare(&s, TEST_KEY, "a.txt", "some bytes"), declare(&s, TEST_KEY, "a.txt", "some bytes"));
    assert_ne!(str_of(&a, "slug"), str_of(&b, "slug"));
    let one = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    let two = declare_keyed(&s, TEST_KEY, Some("push-2"), "a.txt");
    assert_ne!(slug(&one), slug(&two));
}

/// One client's "push-1" does not collide with another's, while one key does
/// cover the run and the artifact of a single push.
#[test]
fn a_key_is_scoped_to_its_caller_and_to_what_it_creates() {
    let s = Server::new();
    let mine = declare_keyed(&s, "krowk_sk_mine", Some("push-1"), "a.txt");
    let theirs = declare_keyed(&s, "krowk_sk_theirs", Some("push-1"), "a.txt");
    assert_eq!(theirs.status, 201);
    assert_ne!(slug(&mine), slug(&theirs));
    assert_eq!(open_run_keyed(&s, TEST_KEY, Some("push-9"), "abc").status, 201);
    assert_eq!(declare_keyed(&s, TEST_KEY, Some("push-9"), "a.txt").status, 201);
}

/// Asked for and not delivered is worse than not asked for.
#[test]
fn an_empty_key_is_refused_rather_than_ignored() {
    let s = Server::new();
    for key in ["", "   "] {
        let r = declare_keyed(&s, TEST_KEY, Some(key), "a.txt");
        assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"), "{key:?}");
    }
    let r = open_run_keyed(&s, TEST_KEY, Some(""), "abc");
    assert_eq!(r.code(), "parameter_missing");
}

/// A key outlives its artifact's lifecycle: a replay will not presign a
/// taken-down, finalized or expired one.
#[test]
fn a_replay_will_not_presign_what_is_gone_or_ready() {
    let s = Server::new();
    let down = declare_keyed(&s, TEST_KEY, Some("down"), "a.txt");
    assert_eq!(take_down(&s, TEST_KEY, &slug(&down), "").status, 204);
    let r = declare_keyed(&s, TEST_KEY, Some("down"), "a.txt");
    assert_eq!((r.status, r.code().as_str()), (410, "taken_down"));

    let ready = declare_keyed(&s, TEST_KEY, Some("ready"), "a.txt").json();
    assert_eq!(put(&upload_url(&ready), "the bytes"), 200);
    assert_eq!(finalize(&s, TEST_KEY, &ready).status, 200);
    let r = declare_keyed(&s, TEST_KEY, Some("ready"), "a.txt");
    assert_eq!((r.status, r.code().as_str()), (409, "already_finalized"));

    declare_keyed(&s, "", Some("lapsed"), "a.txt");
    s.advance(EPHEMERAL_LIFETIME + SignedDuration::from_nanos(1));
    let r = declare_keyed(&s, "", Some("lapsed"), "a.txt");
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
}

/// Honoured for at least a day, as the registry promises.
#[test]
fn a_key_is_still_honoured_a_day_later() {
    let s = Server::new();
    let first = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    s.advance(SignedDuration::from_hours(25));
    let second = declare_keyed(&s, TEST_KEY, Some("push-1"), "a.txt");
    assert_eq!((second.status, slug(&second)), (201, slug(&first)));
}

/// A run replays however far through its lifecycle it has gone, and refuses
/// different metadata.
#[test]
fn one_key_opens_one_run() {
    let s = Server::new();
    let first = open_run_keyed(&s, TEST_KEY, Some("push-1"), "abc");
    let second = open_run_keyed(&s, TEST_KEY, Some("push-1"), "abc");
    assert_eq!((second.status, slug(&second)), (201, slug(&first)));
    let r = open_run_keyed(&s, TEST_KEY, Some("push-1"), "def");
    assert_eq!((r.status, r.code().as_str()), (409, "idempotency_key_reused"));
    let run = slug(&first);
    assert_eq!(request("PUT", &s.at(&format!("/v1/runs/{run}/completion")), TEST_KEY, "", "").status, 200);
    let again = open_run_keyed(&s, TEST_KEY, Some("push-1"), "abc");
    assert_eq!((again.status, slug(&again), again.json()["status"].as_str()), (201, run, Some("finished")));
}

/// The registry digests the declared payload as sent, so the same key at
/// another visibility — or with the default spelled out — is another request.
#[test]
fn reusing_a_key_at_another_visibility_is_refused() {
    let s = Server::new();
    let declared = |key: &str, vis: Option<&str>| {
        let mut a = json!({"filename": "shot.png", "content_type": "image/png", "byte_size": 5});
        if let Some(v) = vis {
            a["visibility"] = json!(v);
        }
        keyed("POST", &s.at("/v1/artifacts"), "krowk_sk_owner", Some(key), &json!({"artifact": a}).to_string())
    };
    assert_eq!(declared("one-attempt", Some("public")).status, 201);
    assert_eq!(declared("one-attempt", Some("private")).code(), "idempotency_key_reused");
    assert_eq!(declared("another-attempt", None).status, 201);
    assert_eq!(declared("another-attempt", Some("public")).code(), "idempotency_key_reused");
}

/// What a key is matched on: the permitted parameters, canonicalized — key order
/// and whitespace are not the request, an absent parameter is not an empty one,
/// and a number is the literal it was written as.
#[test]
fn the_digest_is_the_declared_object_canonicalized() {
    let s = Server::new();
    let declared = |key: &str, body: &str| keyed("POST", &s.at("/v1/artifacts"), "krowk_sk_owner", Some(key), body);
    let base = r#""filename":"shot.png","content_type":"image/png","byte_size":5"#;
    let made = declared("re", &format!(r#"{{"artifact":{{{base},
        "metadata":{{"b":"2","a":"1"}}}}}}"#));
    assert_eq!(made.status, 201);
    let replayed = declared("re", r#"{"artifact":{"metadata":{"a":"1","b":"2"},"byte_size":5,"content_type":"image/png","filename":"shot.png"}}"#);
    assert_eq!(slug(&replayed), slug(&made));
    assert_eq!(declared("re", &format!(r#"{{"artifact":{{{base},"metadata":{{"a":"1","b":"3"}}}}}}"#)).code(), "idempotency_key_reused");

    assert_eq!(declared("empty", &format!(r#"{{"artifact":{{{base}}}}}"#)).status, 201);
    assert_eq!(declared("empty", &format!(r#"{{"artifact":{{{base},"checksum":""}}}}"#)).code(), "idempotency_key_reused");

    assert_eq!(declared("n", &format!(r#"{{"artifact":{{{base},"metadata":{{"n":9007199254740993}}}}}}"#)).status, 201);
    assert_eq!(declared("n", &format!(r#"{{"artifact":{{{base},"metadata":{{"n":9007199254740992}}}}}}"#)).code(), "idempotency_key_reused");
    assert_eq!(declared("f", &format!(r#"{{"artifact":{{{base},"metadata":{{"n":1}}}}}}"#)).status, 201);
    assert_eq!(declared("f", &format!(r#"{{"artifact":{{{base},"metadata":{{"n":1.0}}}}}}"#)).code(), "idempotency_key_reused");

    let first = declared("unread", &format!(r#"{{"artifact":{{{base}}}}}"#));
    let tolerated = declared("unread", &format!(r#"{{"artifact":{{{base},"nonsense":"ignored"}}}}"#));
    assert_eq!((tolerated.status, slug(&tolerated)), (201, slug(&first)));
}
