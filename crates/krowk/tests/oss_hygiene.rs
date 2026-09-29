//! What an open-source security tool owes the people who read its source
//! (spec D18), held where a test can hold it: no telemetry crate in either
//! build, a disclosure policy that says where to report, no vendor's name
//! on anything krowk ships under its own name, and a `cargo deny` policy
//! that cannot be loosened quietly.
//!
//! The rest of D18 is checked elsewhere. The public threat model and
//! crypto design (R-OSS-1) are Canon's `engineering/threat-model.md` and
//! `engineering/crypto.md`, and the audit they scope (R-OSS-2) is ticket
//! 37. Signing, the SBOM and the reproducible rebuild are steps of
//! `.github/workflows/release.yml`, `cargo deny` itself is CI's `deny` job,
//! and the lean build's dependency promise (R-OSS-7) is
//! `scripts/lean_deps_check.sh`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `cargo metadata` for the whole workspace, every feature on, every target.
fn metadata() -> Value {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked", "--all-features"])
        .current_dir(root())
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON")
}

/// Crates whose job is to send what a program does somewhere else: error
/// and crash reporters, analytics clients, trace and metric exporters.
/// OpenTelemetry's *names* are fine — `runctx.rs` stamps artifacts with its
/// attribute keys — but none of its exporters may be linked.
const REPORTERS: &[&str] = &["sentry", "posthog", "segment", "rudderstack", "mixpanel", "amplitude", "bugsnag", "rollbar", "honeycomb", "datadog", "newrelic", "opentelemetry-otlp", "opentelemetry-http", "opentelemetry_sdk", "crash-handler", "minidump", "breakpad"];

/// R-OSS-4: krowk has no telemetry and no crash reporting, so there is
/// nothing to opt in to, and SECURITY.md says so. This keeps it true: no
/// crate that reports anywhere is in either build's graph, on any target.
#[test]
fn r_oss_4_no_telemetry_or_crash_reporter_is_linked() {
    let meta = metadata();
    let linked: Vec<&str> = meta["packages"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).filter(|n| REPORTERS.iter().any(|r| n == r || n.starts_with(&format!("{r}-")) || n.starts_with(&format!("{r}_")))).collect();
    assert!(linked.is_empty(), "a telemetry or crash-reporting crate is in the graph: {linked:?}. krowk sends nothing unasked (SECURITY.md → Telemetry); an opt-in one needs its schema documented there first");
    let security = read("SECURITY.md");
    assert!(security.contains("## Telemetry"), "SECURITY.md documents what krowk sends unasked");
}

/// R-OSS-5: a disclosure policy, with a private address to report to.
#[test]
fn r_oss_5_security_md_says_where_to_report() {
    let security = read("SECURITY.md");
    assert!(security.contains("## Reporting a vulnerability"), "SECURITY.md has a reporting section");
    assert!(security.contains("security@krowk.com"), "SECURITY.md names the private address");
}

/// Names that belong to someone else's product.
const VENDORS: &[&str] = &["anthropic", "claude", "openai", "codex", "chatgpt", "gpt", "cursor", "grok", "xai", "gemini", "google", "copilot", "github"];

fn vendor_in(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    VENDORS.iter().copied().find(|v| lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| word == *v))
}

/// R-OSS-6: nothing krowk ships under its own name — a crate, a binary, a
/// cargo feature, an npm package — carries a vendor's name. Code that
/// drives a vendor's tool may say whose tool it is (`claude/mod.rs` drives
/// Claude Code); what a person installs, enables or runs is krowk's.
#[test]
fn r_oss_6_no_vendor_name_in_a_crate_binary_feature_or_package() {
    let meta = metadata();
    let members: Vec<&str> = meta["workspace_members"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let mut names = Vec::new();
    for pkg in meta["packages"].as_array().unwrap().iter().filter(|p| members.contains(&p["id"].as_str().unwrap_or_default())) {
        let crate_name = pkg["name"].as_str().unwrap().to_string();
        names.push(format!("crate {crate_name}"));
        for target in pkg["targets"].as_array().unwrap() {
            if target["kind"].as_array().unwrap().iter().any(|k| k == "bin") {
                names.push(format!("binary {}", target["name"].as_str().unwrap()));
            }
        }
        for feature in pkg["features"].as_object().unwrap().keys() {
            names.push(format!("feature {crate_name}/{feature}"));
        }
    }
    for entry in std::fs::read_dir(root().join("npm")).unwrap().flatten() {
        let manifest = entry.path().join("package.json");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            let pkg: Value = serde_json::from_str(&text).unwrap();
            names.push(format!("npm {}", pkg["name"].as_str().unwrap()));
        }
    }
    assert!(names.iter().any(|n| n == "binary krowk"), "the walk found krowk's own binary: {names:?}");
    let offending: Vec<String> = names.iter().filter_map(|n| vendor_in(n).map(|v| format!("{n} ({v})"))).collect();
    assert!(offending.is_empty(), "a name krowk ships under carries a vendor's: {offending:?}");
}

/// R-OSS-3: the `cargo deny` policy CI runs cannot be loosened by an edit
/// that looks harmless. Every advisory is an error, every exception names
/// its advisory and says why, and crates come from crates.io only.
#[test]
fn r_oss_3_the_cargo_deny_policy_refuses_advisories_and_unknown_sources() {
    let deny = read("deny.toml");
    let lines: Vec<&str> = deny.lines().map(str::trim).filter(|l| !l.starts_with('#')).collect();
    for want in ["unmaintained = \"all\"", "yanked = \"deny\"", "unknown-registry = \"deny\"", "unknown-git = \"deny\""] {
        assert!(lines.contains(&want), "deny.toml keeps `{want}`");
    }
    // Levels that would turn an advisory into a warning.
    for loosened in ["vulnerability = ", "unsound = ", "notice = ", "severity-threshold"] {
        assert!(!lines.iter().any(|l| l.starts_with(loosened)), "deny.toml sets `{loosened}…`: advisories are errors, and an exception is an `ignore` entry with a reason");
    }
    for exception in lines.iter().filter(|l| l.starts_with("{ id = \"RUSTSEC-")) {
        assert!(exception.contains("reason = \""), "an advisory exception says why: {exception}");
    }
    let release = read(".github/workflows/release.yml");
    let ci = read(".github/workflows/ci.yml");
    assert!(ci.contains("run: cargo deny check") && release.contains("run: cargo deny check"), "CI and the release both run cargo deny");
}
