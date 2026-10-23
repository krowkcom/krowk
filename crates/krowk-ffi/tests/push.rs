//! R-IOS-12, R-CLIENT-1: the paired phone registers its APNs token with
//! the registry and clears it, through `Account`, signed in Rust by its
//! device key — never in Swift. Against the stand-in registry, read back
//! through its `GET /_push_tokens`, which never shows a token.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use common::World;
use krowk_ffi::KrowkError;
use serde_json::{Value, json};

/// What the stand-in says is registered: each device with a token, and
/// the environment it is for.
fn registered(w: &World) -> Value {
    let url = w.registry.url();
    let addr = url.trim_start_matches("http://").trim_end_matches('/');
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET /_push_tokens HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let (_, body) = raw.split_once("\r\n\r\n").expect("a body");
    assert!(!body.contains("cd".repeat(32).as_str()) && !body.contains("ef".repeat(32).as_str()), "a token shown: {body}");
    serde_json::from_str(body).unwrap()
}

#[test]
fn r_ios_12_the_phone_sets_replaces_and_clears_its_push_token() {
    let w = World::new();
    let phone = w.phone.device_id();
    assert_eq!(registered(&w), json!({ "push_tokens": [] }));

    w.phone.set_push_token("cd".repeat(32), "sandbox".into()).unwrap();
    assert_eq!(registered(&w), json!({ "push_tokens": [{ "device": phone, "environment": "sandbox" }] }));
    // A rotated token, or a build for the other APNs host, replaces it.
    w.phone.set_push_token("ef".repeat(32), "production".into()).unwrap();
    assert_eq!(registered(&w), json!({ "push_tokens": [{ "device": phone, "environment": "production" }] }));

    w.phone.clear_push_token().unwrap();
    assert_eq!(registered(&w), json!({ "push_tokens": [] }));
    // Clearing with none is fine: a denial seen twice clears twice.
    w.phone.clear_push_token().unwrap();
    assert_eq!(registered(&w), json!({ "push_tokens": [] }));
}

#[test]
fn r_ios_12_a_bad_token_or_environment_is_refused_and_nothing_is_kept() {
    let w = World::new();
    for (token, environment) in [("cd".repeat(31), "sandbox"), ("xy".repeat(32), "sandbox"), ("cd".repeat(32), "staging")] {
        match w.phone.set_push_token(token, environment.into()) {
            Err(KrowkError::Refused { code, detail }) => {
                assert_eq!(code, "invalid", "{environment}: {detail}");
                assert!(!detail.contains(&"cd".repeat(31)), "the token is not in the error: {detail}");
            }
            other => panic!("{environment}: {other:?}"),
        }
    }
    assert_eq!(registered(&w), json!({ "push_tokens": [] }));
}
