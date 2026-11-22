//! Runs, attaching an artifact to one, and the pagination every listing shares.

mod common;

use common::*;
use jiff::SignedDuration;

/// A run body that does not parse is refused as a declare's is; an empty one
/// is fine.
#[test]
fn a_garbage_run_body_is_rejected() {
    let s = Server::new();
    let r = request("POST", &s.at("/v1/runs"), TEST_KEY, "application/json", r#"{"run": not json"#);
    assert_eq!((r.status, r.code().as_str()), (400, "bad_request"));
    let r = request("POST", &s.at("/v1/runs"), TEST_KEY, "application/json", r#"{"run": 5}"#);
    assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"));
    assert_eq!(request("POST", &s.at("/v1/runs"), TEST_KEY, "", "").status, 201);
}

/// Completing is idempotent and keeps the moment it first finished.
#[test]
fn finishing_a_run_twice_keeps_the_first_finished_at() {
    let s = Server::new();
    let slug = open_run(&s, TEST_KEY);
    let complete = || request("PUT", &s.at(&format!("/v1/runs/{slug}/completion")), TEST_KEY, "", "");
    let first = complete();
    assert_eq!((first.status, first.json()["status"].as_str()), (200, Some("finished")));
    assert!(first.json()["finished_at"].is_string());
    s.advance(SignedDuration::from_hours(1));
    assert_eq!(complete().json(), first.json());
}

/// A run's fields go out in declaration order, as the Go struct wrote them.
#[test]
fn a_run_serializes_in_declaration_order() {
    let s = Server::new();
    let r = request("POST", &s.at("/v1/runs"), TEST_KEY, "application/json", r#"{"run":{"metadata":{"b":1, "a":2.50}}}"#);
    let text = r.text();
    let order: Vec<usize> = ["slug", "status", "started_at", "finished_at", "metadata", "created_at"]
        .iter()
        .map(|k| text.find(&format!("\"{k}\"")).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    assert!(text.contains("\"metadata\": {\n    \"b\": 1,\n    \"a\": 2.50\n  }"), "metadata is sent as it came:\n{text}");
}

/// Attaching after the fact is how an anonymous upload ever gets a run. PUT and
/// PATCH, idempotent, the link unmoved, and a finished run still accepts one.
#[test]
fn attach_run_puts_an_owned_artifact_under_a_run() {
    let s = Server::new();
    let key = "krowk_sk_owner";
    let artifact = declare(&s, key, "shot.png", "some bytes");
    let slug = str_of(&artifact, "slug");
    let run = open_run(&s, key);
    for _ in 0..2 {
        let r = attach(&s, key, slug, &run);
        assert_eq!((r.status, run_slug_of(&r.json())), (200, run.as_str()));
    }
    let url = s.at(&format!("/v1/artifacts/{slug}/run"));
    let r = request("PATCH", &url, key, "application/json", &format!(r#"{{"run":"{run}"}}"#));
    assert_eq!((r.status, run_slug_of(&r.json())), (200, run.as_str()));
    assert_eq!(must_show(&s, key, slug)["url"], artifact["url"]);
    let nested = &must_show(&s, key, slug)["run"];
    assert!(nested["created_at"].is_string() && nested["metadata"] == serde_json::json!({}), "{nested}");

    assert_eq!(request("PUT", &s.at(&format!("/v1/runs/{run}/completion")), key, "", "").status, 200);
    let later = declare(&s, key, "later.png", "some bytes");
    let r = attach(&s, key, str_of(&later, "slug"), &run);
    assert_eq!((r.status, run_slug_of(&r.json())), (200, run.as_str()));
}

#[test]
fn attach_run_needs_a_key_and_a_run() {
    let s = Server::new();
    let key = "krowk_sk_owner";
    let slug = str_of(&declare(&s, key, "shot.png", "some bytes"), "slug").to_owned();
    let run = open_run(&s, key);
    let r = attach(&s, "", &slug, &run);
    assert_eq!((r.status, r.code().as_str()), (401, "unauthorized"));
    let r = request("PUT", &s.at(&format!("/v1/artifacts/{slug}/run")), key, "application/json", "{}");
    assert_eq!((r.status, r.code().as_str()), (400, "parameter_missing"));
}

/// Both slugs resolve in the key's workspace, and an unclaimed anonymous upload
/// is in nobody's.
#[test]
fn attach_run_is_scoped_to_the_keys_workspace() {
    let s = Server::new();
    let (owner, stranger) = ("krowk_sk_owner", "krowk_sk_stranger");
    let slug = str_of(&declare(&s, owner, "shot.png", "some bytes"), "slug").to_owned();
    let run = open_run(&s, owner);
    assert_eq!(attach(&s, stranger, &slug, &run).code(), "not_found");
    assert_eq!(attach(&s, owner, &slug, "run_nosuchrunatall00000").code(), "not_found");
    assert_eq!(attach(&s, owner, &slug, &open_run(&s, stranger)).status, 404);
    let anon = declare(&s, "", "anon.png", "some bytes");
    assert_eq!(attach(&s, owner, str_of(&anon, "slug"), &run).status, 404);
}

/// Newest first, "older than this one", and another key sees none of them.
#[test]
fn the_run_listing_pages_newest_first() {
    let s = Server::new();
    let opened: Vec<String> = (0..3).map(|_| open_run(&s, TEST_KEY)).collect();
    let first = request("GET", &s.at("/v1/runs?limit=2"), TEST_KEY, "", "").json();
    assert_eq!(slugs_of(&first, "runs"), [opened[2].clone(), opened[1].clone()]);
    assert_eq!(first["next"], opened[1].as_str());
    let second = request("GET", &s.at(&format!("/v1/runs?limit=2&before={}", opened[1])), TEST_KEY, "", "").json();
    assert_eq!(slugs_of(&second, "runs"), [opened[0].clone()]);
    assert!(second["next"].is_null());
    let theirs = request("GET", &s.at("/v1/runs"), "krowk_sk_theirs", "", "").json();
    assert!(slugs_of(&theirs, "runs").is_empty());
}

#[test]
fn show_run_is_scoped_to_the_keys_workspace() {
    let s = Server::new();
    let r = request("POST", &s.at("/v1/runs"), "krowk_sk_mine", "application/json", r#"{"run":{"metadata":{"repo":"acme/storefront"}}}"#);
    let slug = str_of(&r.json(), "slug").to_owned();
    let url = s.at(&format!("/v1/runs/{slug}"));
    assert_eq!(request("GET", &url, "krowk_sk_mine", "", "").json()["metadata"]["repo"], "acme/storefront");
    assert_eq!(request("GET", &url, "krowk_sk_theirs", "", "").status, 404);
    assert_eq!(request("GET", &url, "", "", "").status, 401);
}

/// A collection of the run, not a filter: an unknown run is a 404, not an
/// empty page indistinguishable from a run that made nothing.
#[test]
fn run_artifacts_are_a_collection_of_the_run_rather_than_a_filter() {
    let s = Server::new();
    let (mine, other) = (open_run(&s, TEST_KEY), open_run(&s, TEST_KEY));
    let slug = str_of(&declare(&s, TEST_KEY, "a.txt", "aa"), "slug").to_owned();
    assert_eq!(attach(&s, TEST_KEY, &slug, &mine).status, 200);
    declare(&s, TEST_KEY, "loose.txt", "bb");
    let list = |run: &str, token: &str| request("GET", &s.at(&format!("/v1/runs/{run}/artifacts")), token, "", "");
    assert_eq!(slugs_of(&list(&mine, TEST_KEY).json(), "artifacts"), [slug]);
    assert!(slugs_of(&list(&other, TEST_KEY).json(), "artifacts").is_empty());
    assert_eq!(list("run_nosuchrun0000000000", TEST_KEY).status, 404);
    assert_eq!(list(&mine, "krowk_sk_theirs").status, 404);
}

/// `next` is present whenever the page came back full — exactly as many rows as
/// asked for still hands over the cursor.
#[test]
fn a_full_page_always_carries_a_cursor_even_when_it_is_the_last() {
    let s = Server::new();
    let first = open_run(&s, TEST_KEY);
    open_run(&s, TEST_KEY);
    let page = request("GET", &s.at("/v1/runs?limit=2"), TEST_KEY, "", "").json();
    assert_eq!(slugs_of(&page, "runs").len(), 2);
    assert_eq!(page["next"], first.as_str());
    let beyond = request("GET", &s.at(&format!("/v1/runs?limit=2&before={first}")), TEST_KEY, "", "").json();
    assert!(slugs_of(&beyond, "runs").is_empty() && beyond["next"].is_null());
}

/// Clamped rather than refused: more than is served gets the ceiling (100),
/// a non-number the default (50), and even an unrepresentable number clamps.
/// A raw `+` in a query is a space, so `+3` is not a number.
#[test]
fn the_page_size_is_clamped_rather_than_refused() {
    let s = Server::new();
    for _ in 0..102 {
        open_run(&s, TEST_KEY);
    }
    for (limit, want) in [("", 50), ("0", 1), ("-1", 1), ("abc", 50), ("999999999999999999999", 100), ("999", 100), ("%2B3", 3), ("+3", 50)] {
        let q = if limit.is_empty() { String::new() } else { format!("?limit={limit}") };
        let r = request("GET", &s.at(&format!("/v1/runs{q}")), TEST_KEY, "", "");
        assert_eq!(r.status, 200);
        assert_eq!(slugs_of(&r.json(), "runs").len(), want, "limit={limit:?}");
    }
}

/// Scoped three ways: the cursor belongs to this run, an unknown one is a 404,
/// and a tombstone is not something the run still holds.
#[test]
fn a_runs_artifact_listing_is_scoped_to_the_run_and_to_what_it_still_holds() {
    let s = Server::new();
    let (mine, other) = (open_run(&s, TEST_KEY), open_run(&s, TEST_KEY));
    let attached = |run: &str, name: &str| {
        let slug = str_of(&ready_artifact(&s, TEST_KEY, &format!("bytes for {name}")), "slug").to_owned();
        assert_eq!(attach(&s, TEST_KEY, &slug, run).status, 200);
        slug
    };
    let (older, newer, elsewhere) = (attached(&mine, "older"), attached(&mine, "newer"), attached(&other, "elsewhere"));
    let listing = |q: &str| request("GET", &s.at(&format!("/v1/runs/{mine}/artifacts{q}")), TEST_KEY, "", "");
    assert_eq!(listing(&format!("?before={elsewhere}")).status, 404);
    assert_eq!(listing("?before=art_nosuchartifact00001").status, 404);
    assert_eq!(slugs_of(&listing(&format!("?before={newer}")).json(), "artifacts"), std::slice::from_ref(&older));
    assert_eq!(take_down(&s, TEST_KEY, &older, "").status, 204);
    assert_eq!(slugs_of(&listing("").json(), "artifacts"), [newer]);
}
