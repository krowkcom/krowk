//! Routing, unreadable bodies, the card page, slugs, the paste block and the
//! browser login.

mod common;

use common::*;
use jiff::SignedDuration;
use krowk_devregistry::{CLI_AUTHORIZATION_GRACE, CLI_AUTHORIZATION_LIFETIME};

/// An unknown slug and an unknown path are different 404s, and a known path
/// with the wrong verb is the unknown path — not a 405.
#[test]
fn unrouted_paths_are_not_the_same_404_as_unknown_records() {
    let s = Server::new();
    let r = request("GET", &s.at("/v1/nothing-here"), "krowk_sk_owner", "", "");
    assert_eq!((r.status, r.code().as_str()), (404, "no_such_endpoint"));
    let r = request("GET", &s.at("/v1/artifacts/art_nosuchartifact000"), "krowk_sk_owner", "", "");
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    let r = request("PUT", &s.at("/v1/artifacts"), "krowk_sk_owner", "application/json", "{}");
    assert_eq!((r.status, r.code().as_str()), (404, "no_such_endpoint"));
    let r = request("GET", &s.at("/v1/artifacts/"), "krowk_sk_owner", "", "");
    assert_eq!(r.code(), "no_such_endpoint", "a trailing slash is no slug");
}

/// Go's mux redirects a path sent uncleaned, and GET routes answer HEAD.
#[test]
fn paths_are_cleaned_and_head_is_get() {
    let s = Server::new();
    let r = request("GET", &s.at("//v1/./key?x=1"), TEST_KEY, "", "");
    assert_eq!((r.status, r.header("Location")), (307, Some("/v1/key?x=1")));
    assert_eq!(r.text(), "<a href=\"/v1/key?x=1\">Temporary Redirect</a>.\n\n");
    let r = request("GET", &s.at("/_storage"), "", "", "");
    assert_eq!((r.status, r.header("Location")), (307, Some("/_storage/")));
    let r = request("HEAD", &s.at("/"), "", "", "");
    assert_eq!((r.status, r.body.len()), (200, 0));
    assert_eq!(request("GET", &s.at("/"), "", "", "").json(), serde_json::json!({"service": "krowk-registry", "versions": ["v1"]}));
}

/// A body that does not parse has nothing to name a parameter from; an absent
/// one is exactly the missing parameter.
#[test]
fn a_body_that_does_not_parse_is_refused_as_one() {
    let s = Server::new();
    for body in [r#"{"artifact": "#, "{not json at all"] {
        let r = request("POST", &s.at("/v1/artifacts"), "krowk_sk_owner", "application/json", body);
        assert_eq!((r.status, r.code().as_str()), (400, "bad_request"), "{body}");
    }
    let r = request("POST", &s.at("/v1/artifacts"), "krowk_sk_owner", "application/json", "");
    assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"));
}

/// An artifact parameter carrying nothing the API reads is the parameter absent.
#[test]
fn an_artifact_parameter_carrying_nothing_reads_as_missing() {
    let s = Server::new();
    for body in ["{}", r#"{"artifact":null}"#, r#"{"artifact":{}}"#, r#"{"artifact":{"nonsense":1}}"#] {
        let r = request("POST", &s.at("/v1/artifacts"), "krowk_sk_owner", "application/json", body);
        assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"), "{body}");
    }
}

/// The byte ceiling and the blank fields are validation, named field by field.
#[test]
fn a_declare_is_validated_field_by_field() {
    let s = Server::with_limit(4);
    let d = |body: &str| request("POST", &s.at("/v1/artifacts"), "", "application/json", body).json();
    let big = d(r#"{"artifact":{"filename":"a","content_type":"t","byte_size":5}}"#);
    assert_eq!(big["error"]["details"]["byte_size"][0], "must be at most 4 bytes");
    assert_eq!(big["error"]["message"], "Byte size must be at most 4 bytes");
    let blank = d(r#"{"artifact":{"content_type":"t","byte_size":1}}"#);
    assert_eq!(blank["error"]["message"], "Filename can't be blank");
}

/// og:image names the bytes, and only once they have landed; a pending card
/// says so, and a slug nobody minted is a 404.
#[test]
fn the_card_page_carries_the_open_graph_tags() {
    let s = Server::new();
    let payload = declare_typed(&s, TEST_KEY, "shot.png", "image/png", 10);
    let card = str_of(&payload, "url");
    let pending = request("GET", card, "", "", "");
    assert!(pending.status == 200 && pending.text().contains("pending"));
    assert!(!pending.text().contains(r#"property="og:image""#));

    assert_eq!(request("PUT", &upload_url(&payload), "", "image/png", "some bytes").status, 200);
    assert_eq!(finalize(&s, TEST_KEY, &payload).status, 200);
    let page = request("GET", card, "", "", "");
    assert_eq!(page.header("Content-Type"), Some("text/html; charset=utf-8"));
    let file_url = str_of(&payload, "file_url");
    for want in [
        format!(r#"<meta property="og:image" content="{file_url}">"#),
        r#"<meta property="og:title" content="shot.png">"#.to_owned(),
        format!(r#"<meta property="og:url" content="{card}">"#),
        r#"<meta property="og:description" content="10 B · krowk">"#.to_owned(),
    ] {
        assert!(page.text().contains(&want), "card is missing {want}:\n{}", page.text());
    }
    // The bytes are served with the type they were stored with.
    assert_eq!(request("GET", file_url, "", "", "").header("Content-Type"), Some("image/png"));
    assert_eq!(request("GET", &s.at("/a/art_nosuchartifact00000"), "", "", "").status, 404);

    let log = declare(&s, TEST_KEY, "build.log", "some bytes");
    assert!(!request("GET", str_of(&log, "url"), "", "", "").text().contains("og:image"));
}

/// Prefix plus 24 lowercase base36, random rather than sequential.
#[test]
fn minted_slugs_have_the_canonical_shape() {
    let s = Server::new();
    let shaped = |v: &str| {
        let (prefix, rest) = v.split_once('_').unwrap();
        ["art", "run", "ws"].contains(&prefix) && rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    };
    let artifact = str_of(&declare(&s, TEST_KEY, "shot.png", "some bytes"), "slug").to_owned();
    let run = open_run(&s, TEST_KEY);
    let key = request("GET", &s.at("/v1/key"), TEST_KEY, "", "").json();
    for slug in [artifact.as_str(), &run, str_of(&key, "workspace"), "ws_anonymous000000000000000"] {
        assert!(shaped(slug), "{slug} is not prefix + 24 lowercase base36");
    }
    assert_ne!(str_of(&declare(&s, TEST_KEY, "shot.png", "some bytes"), "slug"), artifact);
    assert_eq!(request("GET", &s.at("/v1/key"), "", "", "").status, 401);
}

/// Every reference has one silhouette: an image embeds and clicks through, a
/// caption from the metadata names it, and anything else is the bolded line.
#[test]
fn the_paste_block_is_built_from_the_artifact() {
    let s = Server::new();
    let body = r#"{"artifact":{"filename":"shot.png","content_type":"image/png","byte_size":4,
        "metadata":{"krowk.caption":"Button update before"}}}"#;
    let p = request("POST", &s.at("/v1/artifacts"), "hunter2", "application/json", body).json();
    let (url, file) = (str_of(&p, "url"), str_of(&p, "file_url"));
    let want = format!("[![Button update before]({file})]({url})\nButton update before · [View preview ↗]({url})");
    assert_eq!(p["paste"]["markdown"], want.as_str());
    assert_eq!(p["paste"]["url"], url);

    let p = declare_typed(&s, "hunter2", "frame[0].png", "image/png", 5);
    assert!(str_of(&p["paste"], "markdown").contains(r"[![frame\[0\].png]("));
    let p = declare(&s, "hunter2", "deploy.log", "build output");
    assert_eq!(p["paste"]["markdown"], format!("**deploy.log** · [View preview ↗]({})", str_of(&p, "url")).as_str());
    let destinations = &p["paste"]["destinations"];
    for (d, want) in [("github", "markdown"), ("linear", "markdown"), ("slack", "url"), ("asana", "url"), ("_default", "markdown")] {
        assert_eq!(destinations[d], want, "{d}");
    }
    let p = declare_typed(&s, "", "shot.png", "image/png", 5);
    assert!(str_of(&p["paste"], "markdown").contains(" · expires "), "{p}");
}

/// A login says what it is for, and whether it asks for the password
/// again; one that names nothing, or something unknown, is a plain login.
#[test]
fn a_browser_login_carries_its_action_and_fresh() {
    let s = Server::new();
    let r = request("POST", &s.at("/v1/cli/authorizations?action=remove_device&fresh=true"), "", "", "").json();
    assert_eq!((str_of(&r, "action"), r["fresh"].clone()), ("remove_device", serde_json::json!(true)), "{r}");
    let plain = request("POST", &s.at("/v1/cli/authorizations?action=wipe_everything"), "", "", "").json();
    assert_eq!((str_of(&plain, "action"), plain["fresh"].clone()), ("login", serde_json::json!(false)), "{plain}");
}

fn open_login(s: &Server) -> (String, String) {
    let r = request("POST", &s.at("/v1/cli/authorizations"), "", "", "");
    assert_eq!(r.status, 201, "{}", r.text());
    let p = r.json();
    assert!(str_of(&p, "verification_url").ends_with(&format!("/_approve/cli/authorizations/new?code={}", str_of(&p, "code"))));
    assert_eq!(p["interval"], 1);
    (str_of(&p, "slug").to_owned(), str_of(&p, "code").to_owned())
}

fn decide(s: &Server, code: &str, button: &str) -> Response {
    request("POST", &s.at(&format!("/_approve/cli/authorizations/{code}/{button}")), "", "", "")
}

fn poll(s: &Server, slug: &str) -> Response {
    request("GET", &s.at(&format!("/v1/cli/authorizations/{slug}")), "", "", "")
}

/// The key is handed over once, and it is the key the rest of the registry
/// knows. The code only approves; polling by it finds nothing.
#[test]
fn a_browser_login_hands_the_key_over_once() {
    let s = Server::new();
    let (slug, code) = open_login(&s);
    let pending = poll(&s, &slug).json();
    assert!(pending["state"] == "pending" && pending.get("token").is_none(), "{pending}");
    assert_eq!(poll(&s, &code).status, 404, "the code is not the slug");

    let answer = decide(&s, &code, "approval");
    assert_eq!(answer.status, 200);
    assert!(!answer.text().contains("krowk_sk_"), "the approval answered with the key");

    let granted = poll(&s, &slug).json();
    let token = str_of(&granted, "token");
    assert!(token.starts_with("krowk_sk_"), "{granted}");
    let key = request("GET", &s.at("/v1/key"), token, "", "").json();
    assert_eq!((&key["key_id"], &key["workspace"]), (&granted["key_id"], &granted["workspace"]));
    let r = poll(&s, &slug);
    assert_eq!((r.status, r.code().as_str()), (410, "spent"));
}

#[test]
fn a_denied_browser_login_keeps_saying_so() {
    let s = Server::new();
    let (slug, code) = open_login(&s);
    assert_eq!(decide(&s, &code, "denial").status, 200);
    for _ in 0..2 {
        let denied = poll(&s, &slug).json();
        assert!(denied["state"] == "denied" && denied.get("token").is_none(), "{denied}");
    }
    assert_eq!(decide(&s, &code, "approval").status, 409);
}

/// Lapsed is 410, and expiry beats approval; spent outranks expiry.
#[test]
fn a_browser_login_expires_and_spent_outranks_it() {
    let s = Server::new();
    let (slug, code) = open_login(&s);
    let (spent_slug, spent_code) = open_login(&s);
    assert_eq!(decide(&s, &spent_code, "approval").status, 200);
    assert!(poll(&s, &spent_slug).json()["token"].is_string());
    s.advance(CLI_AUTHORIZATION_LIFETIME + SignedDuration::from_secs(1));
    let r = poll(&s, &slug);
    assert_eq!((r.status, r.code().as_str()), (410, "expired"));
    assert_eq!(decide(&s, &code, "approval").status, 410);
    assert_eq!(poll(&s, &spent_slug).code(), "spent");
}

/// The page refuses a code matching nothing rather than offering buttons that
/// would 404 once pressed.
#[test]
fn the_approval_page_refuses_a_code_that_matches_nothing() {
    let s = Server::new();
    let (_, code) = open_login(&s);
    let page = request("GET", &s.at(&format!("/_approve/cli/authorizations/new?code={code}")), "", "", "");
    assert!(page.status == 200 && page.text().contains(&code), "{}", page.text());
    assert!(page.text().contains(&format!(r#"action="/_approve/cli/authorizations/{code}/approval""#)));
    for unknown in ["", "ZZZZ-ZZZZ"] {
        let r = request("GET", &s.at(&format!("/_approve/cli/authorizations/new?code={unknown}")), "", "", "");
        assert_eq!(r.status, 404, "{unknown:?}");
    }
}

/// Swept when a login is opened, but only past the grace period, so a late poll
/// still hears the window closed.
#[test]
fn lapsed_browser_logins_are_swept_but_not_immediately() {
    let s = Server::new();
    let (lapsed, _) = open_login(&s);
    s.advance(CLI_AUTHORIZATION_LIFETIME + SignedDuration::from_mins(1));
    open_login(&s);
    assert_eq!(poll(&s, &lapsed).code(), "expired");
    s.advance(CLI_AUTHORIZATION_GRACE);
    open_login(&s);
    assert_eq!(poll(&s, &lapsed).status, 404);
}
