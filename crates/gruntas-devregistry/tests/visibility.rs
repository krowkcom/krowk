//! Who may read an artifact: declaring a visibility, the share token, the
//! private key shape, and the re-key a change makes.

mod common;

use common::*;
use serde_json::{Value, json};

const OWNER: &str = "krowk_sk_owner";
const STRANGER: &str = "krowk_sk_stranger";

fn declare_visible(s: &Server, token: &str, visibility: &str, filename: &str, body: &str) -> Response {
    let d = json!({"artifact": {"filename": filename, "content_type": "text/plain", "byte_size": body.len(), "visibility": visibility}});
    request("POST", &s.at("/v1/artifacts"), token, "application/json", &d.to_string())
}

/// Declared, uploaded and finalized at a visibility: the only state a change moves.
fn pushed(s: &Server, token: &str, visibility: &str, filename: &str) -> Value {
    let r = declare_visible(s, token, visibility, filename, "bytes");
    assert_eq!(r.status, 201, "declare = {}", r.text());
    let payload = r.json();
    assert_eq!(put_signed(&payload, b"bytes"), 200);
    let r = finalize(s, token, &payload);
    assert_eq!(r.status, 200, "finalize = {}", r.text());
    r.json()
}

fn set_visibility(s: &Server, token: &str, slug: &str, body: &str) -> Response {
    request("PUT", &s.at(&format!("/v1/artifacts/{slug}/visibility")), token, "application/json", body)
}

fn get(url: &str) -> Response {
    request("GET", url, "", "", "")
}

#[test]
fn visibility_is_declared_and_served_on_every_read() {
    let s = Server::new();
    assert_eq!(declare(&s, OWNER, "shot.png", "bytes")["visibility"], "public");
    let private = declare_visible(&s, OWNER, "private", "secret.txt", "bytes").json();
    assert_eq!(private["visibility"], "private");
    assert_eq!(must_show(&s, OWNER, str_of(&private, "slug"))["visibility"], "private");
}

/// Refused rather than downgraded; padded is spelled wrong, blank is unnamed.
#[test]
fn a_visibility_the_api_does_not_take_is_refused_rather_than_downgraded() {
    let s = Server::new();
    for asked in ["secret", "PUBLIC", " private"] {
        let r = declare_visible(&s, OWNER, asked, "shot.png", "bytes");
        assert_eq!((r.status, r.code().as_str()), (422, "visibility_unavailable"), "{asked:?}");
    }
    let r = declare_visible(&s, OWNER, "secret", "shot.png", "bytes");
    assert_eq!(r.json()["error"]["message"], "\"secret\" is not a visibility you can declare. Send one of: public, private, shared.");
    let long = "x".repeat(40);
    let message = declare_visible(&s, OWNER, &long, "a", "b").json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with(&format!("\"{}...\" is not", "x".repeat(27))), "{message}");
    let r = declare_visible(&s, OWNER, "   ", "shot.txt", "bytes");
    assert_eq!((r.status, r.json()["visibility"].as_str()), (201, Some("public")));
}

/// A keyless upload lands in a workspace nobody is a member of, so it cannot
/// be private or shared — and the refusal names the key that would work.
#[test]
fn a_keyless_upload_cannot_be_private_or_shared() {
    let s = Server::new();
    let r = declare_visible(&s, "", "private", "secret.txt", "bytes");
    assert_eq!((r.status, r.code().as_str()), (422, "private_needs_key"));
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("API key"));
    let r = declare_visible(&s, "", "shared", "s.txt", "bytes");
    assert_eq!((r.status, r.code().as_str()), (422, "shared_needs_key"));
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("Authorization: Bearer"));
    let ready = pushed(&s, OWNER, "public", "move.txt");
    let r = set_visibility(&s, "", str_of(&ready, "slug"), r#"{"visibility":"shared"}"#);
    assert_eq!((r.status, r.code().as_str()), (422, "shared_needs_key"));
}

fn token_of(share_url: &str) -> String {
    share_url.split_once("share=").unwrap().1.to_owned()
}

/// share_url is null unless shared; a matching token reads keyless, and a
/// wrong, absent or revoked one reads as never minted.
#[test]
fn shared_artifacts_carry_a_share_url() {
    let s = Server::new();
    assert!(pushed(&s, OWNER, "public", "shot.txt")["share_url"].is_null());
    assert!(pushed(&s, OWNER, "private", "secret.txt")["share_url"].is_null());
    let shared = pushed(&s, OWNER, "shared", "shared.txt");
    let first = str_of(&shared, "share_url").to_owned();
    let slug = str_of(&shared, "slug").to_owned();
    assert!(first.ends_with(&format!("/a/{slug}?share={}", token_of(&first))) && token_of(&first).len() == 36, "{first}");
    assert!(str_of(&shared, "markdown").contains(&first));
    assert_eq!(shared["paste"]["url"], first.as_str());

    let token = token_of(&first);
    assert_eq!(get(&s.at(&format!("/v1/artifacts/{slug}?share={token}"))).status, 200);
    assert_eq!(request("GET", &s.at(&format!("/v1/artifacts/{slug}?share={token}")), STRANGER, "", "").status, 200);
    assert_eq!(show(&s, STRANGER, &slug).code(), "not_found");
    assert_eq!(get(&s.at(&format!("/a/{slug}"))).status, 404);
    let card = get(&first);
    assert!(card.status == 200 && card.text().contains(&first.replace('&', "&amp;")));
    let listing = request("GET", &s.at("/v1/artifacts?limit=100"), STRANGER, "", "");
    assert!(!listing.text().contains(&slug));

    // Leaving clears the token; re-entering mints a fresh one.
    assert_eq!(set_visibility(&s, OWNER, &slug, r#"{"visibility":"public"}"#).status, 200);
    assert!(must_show(&s, OWNER, &slug)["share_url"].is_null());
    let reshared = set_visibility(&s, OWNER, &slug, r#"{"visibility":"shared"}"#).json();
    assert_ne!(str_of(&reshared, "share_url"), first);
    for target in [format!("/v1/artifacts/{slug}"), format!("/v1/artifacts/{slug}?share=krowk_share_000000000000000000000000"), format!("/v1/artifacts/{slug}?share={token}")] {
        assert_eq!(get(&s.at(&target)).code(), "not_found", "{target}");
    }
}

/// A private key is a secret and nothing else; holding it is the whole of the
/// authorization. A public one keeps its old shape.
#[test]
fn a_private_byte_url_names_neither_the_workspace_nor_the_artifact() {
    let s = Server::new();
    let workspace = str_of(&request("GET", &s.at("/v1/key"), OWNER, "", "").json(), "workspace").to_owned();
    let private = pushed(&s, OWNER, "private", "secret.txt");
    let file_url = str_of(&private, "file_url");
    assert!(!file_url.contains(str_of(&private, "slug")) && !file_url.contains(&workspace), "{file_url}");
    let r = get(file_url);
    assert_eq!((r.status, r.text().as_str()), (200, "bytes"));
    let public = pushed(&s, OWNER, "public", "shot.txt");
    let public_url = str_of(&public, "file_url");
    assert!(public_url.contains(str_of(&public, "slug")) && public_url.contains(&workspace), "{public_url}");
}

/// The metadata boundary: a private artifact answers its own workspace only,
/// and everyone else as missing; a public one answers everyone.
#[test]
fn private_metadata_answers_its_own_workspace_and_nobody_else() {
    let s = Server::new();
    let private = str_of(&pushed(&s, OWNER, "private", "secret.txt"), "slug").to_owned();
    let public = str_of(&pushed(&s, OWNER, "public", "shot.txt"), "slug").to_owned();
    for token in [STRANGER, ""] {
        let r = show(&s, token, &private);
        assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
        assert_eq!(show(&s, token, &public).status, 200);
    }
    assert_eq!(show(&s, OWNER, &private).status, 200);
}

#[test]
fn the_private_card_page_is_indistinguishable_from_a_slug_that_never_existed() {
    let s = Server::new();
    let private = pushed(&s, OWNER, "private", "secret.txt");
    let page = get(str_of(&private, "url"));
    assert_eq!(page.status, 404);
    assert_eq!(page.text(), get(&s.at("/a/art_nosuchartifact0000000")).text());
}

/// A change re-keys the bytes both ways and kills the old URL; the slug and card
/// survive, and a public key comes back to exactly where it was.
#[test]
fn a_visibility_change_rekeys_the_bytes_and_kills_the_old_url() {
    let s = Server::new();
    let public = pushed(&s, OWNER, "public", "shot.txt");
    let slug = str_of(&public, "slug");
    let was_public = str_of(&public, "file_url");
    let private = set_visibility(&s, OWNER, slug, r#"{"visibility":"private"}"#).json();
    assert_eq!((private["visibility"].as_str(), &private["url"]), (Some("private"), &public["url"]));
    let now_private = str_of(&private, "file_url");
    assert_ne!(now_private, was_public);
    assert_eq!(get(was_public).status, 404);
    assert_eq!(get(now_private).text(), "bytes");

    let back = set_visibility(&s, OWNER, slug, r#"{"visibility":"public"}"#).json();
    assert_eq!(str_of(&back, "file_url"), was_public);
    assert_eq!(get(now_private).status, 404);
    assert_eq!(get(was_public).text(), "bytes");
}

/// Naming the visibility it already has withdraws nothing — even for a pending
/// artifact, which could not move at all.
#[test]
fn naming_the_visibility_an_artifact_already_has_is_a_no_op() {
    let s = Server::new();
    let private = pushed(&s, OWNER, "private", "secret.txt");
    let again = set_visibility(&s, OWNER, str_of(&private, "slug"), r#"{"visibility":"private"}"#);
    assert_eq!(str_of(&again.json(), "file_url"), str_of(&private, "file_url"));
    assert_eq!(get(str_of(&private, "file_url")).text(), "bytes");

    let pending = declare(&s, OWNER, "later.txt", "bytes");
    let slug = str_of(&pending, "slug");
    assert_eq!(set_visibility(&s, OWNER, slug, r#"{"visibility":"public"}"#).status, 200);
    let r = set_visibility(&s, OWNER, slug, r#"{"visibility":"private"}"#);
    assert_eq!((r.status, r.code().as_str()), (422, "immovable"));
}

/// Every refusal, and which answers first: the workspace scope before the body.
#[test]
fn the_visibility_change_refuses_what_it_cannot_do() {
    let s = Server::new();
    let slug = str_of(&pushed(&s, OWNER, "public", "shot.txt"), "slug").to_owned();
    assert_eq!(set_visibility(&s, "", &slug, r#"{"visibility":"private"}"#).status, 401);
    for body in [r#"{"visibility":"private"}"#, r#"{"visibility":"secret"}"#, "{}"] {
        assert_eq!(set_visibility(&s, STRANGER, &slug, body).code(), "not_found", "{body}");
    }
    for (body, status, code) in [
        ("{}", 400, "parameter_missing"),
        (r#"{"visibility":"   "}"#, 400, "parameter_missing"),
        (r#"{"visibility":"secret"}"#, 422, "visibility_unavailable"),
        (r#"{"visibility":" private"}"#, 422, "visibility_unavailable"),
        ("{", 400, "bad_request"),
    ] {
        let r = set_visibility(&s, OWNER, &slug, body);
        assert_eq!((r.status, r.code().as_str()), (status, code), "{body}");
    }
    assert_eq!(take_down(&s, OWNER, &slug, "").status, 204);
    let r = set_visibility(&s, OWNER, &slug, r#"{"visibility":"private"}"#);
    assert_eq!((r.status, r.code().as_str()), (410, "taken_down"));
    let r = set_visibility(&s, OWNER, &slug, r#"{"visibility":"public"}"#);
    assert_eq!(r.status, 410, "a tombstone keeping its visibility is gone, not a live-looking 200");
}
