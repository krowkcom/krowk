//! The client against the stand-in registry, end to end. The stand-in makes
//! the refusals the real registry makes — bytes that never arrived, a length
//! or digest that is not what was declared — so a client that passes here is
//! exercising the real sequence. The stand-in runs in-process, so nothing
//! has to be built first.

use gruntas_api::spec::{inspect, Spec};
use gruntas_api::Client;
use std::net::TcpListener;

/// The stand-in, served in-process on a port of its own; dropping it stops it.
struct Registry(#[allow(dead_code)] gruntas_devregistry::Running, String);

fn registry() -> Registry {
    let running = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
    let url = format!("{}/v1", running.url());
    Registry(running, url)
}

fn file(name: &str, body: &str) -> Spec {
    let dir = std::env::temp_dir().join(format!("gruntas-api-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    inspect(path.to_str().unwrap()).unwrap()
}

#[test]
fn a_keyless_push_lands_expires_and_is_taken_down_by_its_claim_token() {
    let r = registry();
    let c = Client::new(&r.1, "");
    let a = c.push(&file("a.txt", "hello")).unwrap();
    assert_eq!((a.state.as_str(), a.byte_size, a.filename.as_str()), ("ready", 5, "a.txt"));
    assert!(a.claim_token.starts_with("krowk_claim_") && !a.expires_at.is_empty());
    assert_eq!(c.show_artifact(&a.slug).unwrap().slug, a.slug);
    // A keyless caller holding only the slug cannot take it down.
    assert!(c.take_down_artifact(&a.slug, "").is_err());
    c.take_down_artifact(&a.slug, &a.claim_token).unwrap();
    assert_eq!(c.show_artifact(&a.slug).unwrap_err().code(), "taken_down");
}

#[test]
fn a_keyed_push_claims_attaches_lists_and_finishes() {
    let r = registry();
    let keyless = Client::new(&r.1, "");
    let keyed = Client::new(&r.1, "krowk_sk_test");
    assert!(!keyed.verify_key().unwrap().key_id.is_empty());
    assert_eq!(keyless.verify_key().unwrap_err().status, 401);

    let anonymous = keyless.push(&file("b.txt", "world")).unwrap();
    let claimed = keyed.claim_artifact(&anonymous.slug, &anonymous.claim_token).unwrap();
    assert_eq!(claimed.slug, anonymous.slug);

    let run = keyed.create_run(&serde_json::json!({ "vcs.change.title": "it" })).unwrap();
    assert_eq!(run.status, "open");
    let attached = keyed.attach_run(&claimed.slug, &run.slug).unwrap();
    assert_eq!(attached.run_slug(), run.slug);
    assert_eq!(keyed.list_run_artifacts(&run.slug, "", 0).unwrap().artifacts.len(), 1);
    assert!(keyed.list_artifacts("", 1).unwrap().artifacts.len() <= 1);
    assert_eq!(keyed.finish_run(&run.slug).unwrap().status, "finished");
    assert_eq!(keyed.show_run(&run.slug).unwrap().status, "finished");
    assert_eq!(keyed.list_runs("", 0).unwrap().runs.len(), 1);
    assert_eq!(keyed.show_run("run_000000000000000000000000").unwrap_err().code(), "not_found");
}

#[test]
fn a_private_push_needs_a_key_and_the_root_names_the_service() {
    let r = registry();
    let keyed = Client::new(&r.1, "krowk_sk_test");
    let mut spec = file("c.txt", "secret");
    spec.visibility = "private".into();
    assert_eq!(keyed.push(&spec).unwrap().visibility, "private");
    assert!(!keyed.root().unwrap().service.is_empty());
}

#[test]
fn an_unreachable_registry_is_named_as_such() {
    let c = Client::new("http://127.0.0.1:9/v1", "");
    let e = c.show_artifact("art_x").unwrap_err();
    assert_eq!((e.code().as_str(), e.status), ("network_unreachable", 0));
}
