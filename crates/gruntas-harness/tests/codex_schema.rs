//! R-BACK-3: the schema of `codex app-server` is pinned in `schema/codex/`
//! from the Codex version `schema/codex/VERSION` names, and
//! `scripts/codex_schema.sh --check` — which CI runs against that Codex —
//! fails when the pinned copy is stale. The script itself is held to that
//! here, with a stand-in `codex` that generates exactly the pinned file: a
//! pristine copy passes, a deliberately edited one fails, and a Codex of
//! another version is refused rather than compared.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn pinned() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/codex")
}

const BUNDLE: &str = "codex_app_server_protocol.schemas.json";

/// A `codex` that answers `--version` with `version` and writes the pinned
/// bundle for `app-server generate-json-schema --experimental --out DIR`.
fn fake(dir: &Path, version: &str) -> PathBuf {
    let bin = dir.join("codex");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli {version}'; exit 0; fi\n[ \"$1 $2 $3\" = 'app-server generate-json-schema --experimental' ] || exit 3\n[ -d \"$CODEX_HOME\" ] || exit 4\nmkdir -p \"$5\" && cp '{}' \"$5/{BUNDLE}\"\n",
        pinned().join(BUNDLE).display()
    );
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn check(codex: &Path, dir: &Path) -> (i32, String) {
    let out = Command::new("bash").arg(repo().join("scripts/codex_schema.sh")).arg("--check").env("CODEX", codex).env("CODEX_SCHEMA_DIR", dir).output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn r_back_3_the_schema_staleness_check_fails_on_a_deliberately_edited_pinned_schema() {
    let base = std::env::temp_dir().join(format!("krowk-codex-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("pin")).unwrap();
    let version = std::fs::read_to_string(pinned().join("VERSION")).unwrap().trim().to_string();
    let codex = fake(&base, &version);
    let pin = base.join("pin");
    std::fs::copy(pinned().join("VERSION"), pin.join("VERSION")).unwrap();
    std::fs::copy(pinned().join(BUNDLE), pin.join(BUNDLE)).unwrap();
    let (code, said) = check(&codex, &pin);
    assert_eq!(code, 0, "a pristine copy passes: {said}");

    // One description changed, as a hand edit or a newer Codex would.
    let schema = std::fs::read_to_string(pin.join(BUNDLE)).unwrap();
    let edited = schema.replacen("\"description\": \"", "\"description\": \"(edited) ", 1);
    assert_ne!(edited, schema);
    std::fs::write(pin.join(BUNDLE), edited).unwrap();
    let (code, said) = check(&codex, &pin);
    assert_eq!(code, 1, "{said}");
    assert!(said.contains("is stale against Codex") && said.contains("(edited)"), "names the fix and shows the change: {said}");

    // A Codex that is not the pinned one is not compared at all.
    let other = fake(&base.join("pin"), "0.0.1");
    let (code, said) = check(&other, &pin);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains(&format!("npm install -g @openai/codex@{version}")), "{said}");
    let _ = std::fs::remove_dir_all(&base);
}
