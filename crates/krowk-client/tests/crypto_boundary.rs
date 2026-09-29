//! R-E2E-2, R-CLIENT-1: crypto is implemented once, in `krowk-client`. No
//! other crate in the workspace may name a crypto crate as a dependency —
//! normal, dev or build, on any target — so a primitive cannot be reached
//! for from the harness, the TUI or the CLI and quietly become a second
//! implementation. Modelled on `make lean-deps`: the workspace's declared
//! dependencies, from `cargo metadata --no-deps`, held to a rule.
//!
//! Direct dependencies only, by design: a crate's own dependencies are
//! its business (ring reaches krowk-harness through rustls, x25519-dalek
//! reaches krowk-client through hpke), and what this holds is who may
//! *call* a primitive. A `libcrux-*` crate not listed is caught by prefix.
//!
//! `ring` and `sha2` are not on the list: ring is rustls's provider (TLS,
//! and the PKCE verifier's RNG), and a hash is not end-to-end encryption.
//! Neither may be used to build it; review holds that line, this test
//! holds the crates that could.

use serde_json::Value;
use std::process::Command;

/// The crates that are, or wrap, cryptographic primitives: R-E2E-2's, and
/// the obvious others someone might reach for instead (ciphers, MACs, KDFs,
/// signatures, KEMs, whole protocol stacks and C-library bindings).
const CRYPTO: &[&str] = &[
    // HPKE and its KEM
    "hpke", "hpke-rs", "x25519-dalek", "curve25519-dalek", "x-wing", "ml-kem", "kyber",
    // AEADs and ciphers
    "aead", "chacha20poly1305", "chacha20", "poly1305", "xsalsa20poly1305", "crypto_box", "crypto_secretbox",
    "aes", "aes-gcm", "aes-gcm-siv", "aes-siv", "ccm", "cipher",
    // MACs and KDFs
    "hkdf", "hmac", "blake3", "argon2", "scrypt", "pbkdf2",
    // Signatures and curves
    "ed25519", "ed25519-dalek", "ecdsa", "p256", "p384", "p521", "k256", "rsa",
    // Protocols and whole libraries
    "snow", "age", "sodiumoxide", "libsodium-sys", "libsodium-sys-stable", "dryoc", "orion", "openssl", "openssl-sys",
    "aws-lc-rs", "aws-lc-sys", "boring", "boring-sys", "libcrux", "libcrux-hkdf", "libcrux-chacha20poly1305", "libcrux-ml-kem",
];

/// The one crate allowed them.
const OWNER: &str = "krowk-client";

/// Every (crate, crypto dependency) pair outside the owner, from
/// `cargo metadata --format-version 1 --no-deps` output.
fn violations(metadata: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for pkg in metadata["packages"].as_array().expect("packages") {
        let name = pkg["name"].as_str().unwrap_or_default();
        if name == OWNER {
            continue;
        }
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            // `name` is the crate, whatever the dependency is renamed to.
            let crate_name = dep["name"].as_str().unwrap_or_default();
            if CRYPTO.contains(&crate_name) || crate_name.starts_with("libcrux") {
                let kind = dep["kind"].as_str().unwrap_or("normal");
                out.push(format!("{name} depends on {crate_name} ({kind})"));
            }
        }
    }
    out
}

fn metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let out = Command::new(cargo).args(["metadata", "--format-version", "1", "--no-deps", "--offline", "--manifest-path", root]).output().expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON")
}

#[test]
fn r_e2e_2_r_client_1_no_crate_but_krowk_client_imports_a_crypto_primitive() {
    let m = metadata();
    let names: Vec<&str> = m["packages"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    // The workspace is what it should be, so an empty answer means something.
    for crate_name in ["krowk", "krowk-harness", "krowk-tui", "krowk-api", OWNER] {
        assert!(names.contains(&crate_name), "{crate_name} is not in the workspace: {names:?}");
    }
    let owner = m["packages"].as_array().unwrap().iter().find(|p| p["name"] == OWNER).unwrap();
    let deps: Vec<&str> = owner["dependencies"].as_array().unwrap().iter().filter_map(|d| d["name"].as_str()).collect();
    assert!(deps.contains(&"hpke") && deps.contains(&"chacha20poly1305") && deps.contains(&"ed25519-dalek"), "the owner is where the crypto is: {deps:?}");
    let found = violations(&m);
    assert!(found.is_empty(), "crypto is implemented only in krowk-client (R-E2E-2): {found:?} — use krowk_client::e2e instead");
}

/// The check itself fails when it should: the real metadata with a crypto
/// crate added as a normal, a dev, a build and a target-specific
/// dependency, under a rename, and by the libcrux prefix.
#[test]
fn r_e2e_2_the_crypto_import_test_fails_when_another_crate_adds_one() {
    let mut m = metadata();
    for p in m["packages"].as_array_mut().unwrap() {
        let add = match p["name"].as_str() {
            Some("krowk-harness") => serde_json::json!({"name": "chacha20poly1305", "kind": null, "rename": null}),
            Some("krowk-tui") => serde_json::json!({"name": "x25519-dalek", "kind": "dev", "rename": null}),
            Some("krowk") => serde_json::json!({"name": "hpke", "kind": null, "rename": "not_crypto"}),
            Some("krowk-api") => serde_json::json!({"name": "aes-gcm", "kind": "build", "rename": null}),
            Some("krowk-store") => serde_json::json!({"name": "hmac", "kind": null, "rename": null, "target": "cfg(windows)"}),
            Some("krowk-import") => serde_json::json!({"name": "libcrux-sha2", "kind": null, "rename": null}),
            _ => continue,
        };
        p["dependencies"].as_array_mut().unwrap().push(add);
    }
    let found = violations(&m);
    assert_eq!(found.len(), 6, "{found:?}");
    assert!(found.contains(&"krowk-api depends on aes-gcm (build)".to_string()), "{found:?}");
    assert!(found.contains(&"krowk-store depends on hmac (normal)".to_string()), "a target-specific dependency: {found:?}");
    assert!(found.contains(&"krowk-import depends on libcrux-sha2 (normal)".to_string()), "{found:?}");
    assert!(found.contains(&"krowk-harness depends on chacha20poly1305 (normal)".to_string()), "{found:?}");
    assert!(found.contains(&"krowk-tui depends on x25519-dalek (dev)".to_string()), "{found:?}");
    assert!(found.contains(&"krowk depends on hpke (normal)".to_string()), "{found:?}");
}
