//! R-CLIENT-1: `krowk-client` is what the desktop app (gpui) and the
//! phones (UniFFI, ticket 33) link, and they link it without the engine,
//! an async runtime or an HTTP stack. So nothing it pulls in — directly or
//! through another crate, on any target — may be `krowk-harness`, `tokio`
//! or `reqwest`.
//!
//! Unlike the crypto boundary this holds the whole graph, from the
//! resolved `cargo metadata`: a runtime that arrives through krowk-api is
//! linked all the same. Dev-dependencies are left out, since no client
//! links a test's.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// What a client links without.
const FORBIDDEN: &[&str] = &["krowk-harness", "tokio", "reqwest"];

const ROOT: &str = "krowk-client";

/// Each forbidden crate `ROOT` reaches over normal and build edges, with
/// the path it is reached by, from `cargo metadata --format-version 1`.
fn violations(metadata: &Value) -> Vec<String> {
    let names: HashMap<&str, &str> = metadata["packages"].as_array().expect("packages").iter().map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap())).collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"].as_array().expect("resolve").iter().map(|n| (n["id"].as_str().unwrap(), n)).collect();
    let root = names.iter().find(|(_, n)| **n == ROOT).map(|(id, _)| *id).expect("krowk-client is in the graph");
    let mut out = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut stack = vec![(root, ROOT.to_string())];
    while let Some((id, path)) = stack.pop() {
        for dep in nodes[id]["deps"].as_array().into_iter().flatten() {
            let linked = dep["dep_kinds"].as_array().into_iter().flatten().any(|k| k["kind"].as_str() != Some("dev"));
            let dep_id = dep["pkg"].as_str().unwrap();
            if !linked || !seen.insert(dep_id) {
                continue;
            }
            let name = names[dep_id];
            let path = format!("{path} -> {name}");
            // Named where it first comes in, not again for what it brings.
            if FORBIDDEN.contains(&name) {
                out.push(path);
                continue;
            }
            stack.push((dep_id, path));
        }
    }
    out.sort();
    out
}

/// Not `--offline`, unlike the crypto boundary's `--no-deps` call: the
/// resolved graph needs every locked package's manifest, other targets'
/// included, which a runner that built for one target has not fetched.
fn metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let out = Command::new(cargo).args(["metadata", "--format-version", "1", "--manifest-path", root]).output().expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON")
}

#[test]
fn r_client_1_krowk_client_links_no_harness_runtime_or_http_stack() {
    let m = metadata();
    // The forbidden crates are in the workspace's graph, so their absence
    // from krowk-client's means something.
    let names: Vec<&str> = m["packages"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    for name in FORBIDDEN {
        assert!(names.contains(name), "{name} is not in the workspace's graph: the test proves nothing");
    }
    let found = violations(&m);
    assert!(found.is_empty(), "krowk-client links none of {FORBIDDEN:?} (R-CLIENT-1): {found:?}");
}

/// The check fails when it should: the real graph with tokio added to
/// krowk-client directly, reqwest reached through krowk-api as a build
/// dependency, and krowk-harness on one target — and a dev-dependency on
/// tokio, which no client links, not counted.
#[test]
fn r_client_1_the_dependency_graph_test_fails_when_krowk_client_gains_one() {
    let mut m = metadata();
    let id = |m: &Value, name: &str| m["packages"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()["id"].as_str().unwrap().to_string();
    let (client, api, tokio, reqwest, harness) = (id(&m, ROOT), id(&m, "krowk-api"), id(&m, "tokio"), id(&m, "reqwest"), id(&m, "krowk-harness"));
    assert!(violations(&m).is_empty(), "the real graph is clean first");
    let add = |m: &mut Value, from: &str, to: &str, kind: Value, target: Value| {
        let node = m["resolve"]["nodes"].as_array_mut().unwrap().iter_mut().find(|n| n["id"] == from).unwrap();
        node["deps"].as_array_mut().unwrap().push(serde_json::json!({"name": "x", "pkg": to, "dep_kinds": [{"kind": kind, "target": target}]}));
    };

    add(&mut m, &client, &tokio, Value::Null, Value::Null);
    add(&mut m, &api, &reqwest, "build".into(), Value::Null);
    add(&mut m, &client, &harness, Value::Null, "cfg(windows)".into());
    let found = violations(&m);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found.contains(&"krowk-client -> tokio".to_string()), "{found:?}");
    assert!(found.contains(&"krowk-client -> krowk-api -> reqwest".to_string()), "a transitive build dependency: {found:?}");
    assert!(found.contains(&"krowk-client -> krowk-harness".to_string()), "a target-specific dependency: {found:?}");

    let mut dev = metadata();
    add(&mut dev, &client, &tokio, "dev".into(), Value::Null);
    assert!(violations(&dev).is_empty(), "a dev-dependency is not linked: {:?}", violations(&dev));
}
