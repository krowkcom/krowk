//! R-PROTO-2: the protocol's JSON Schema is generated from the Rust types,
//! and the copy checked in beside the crate is never stale. A type change
//! fails this until `make schema` (KROWK_SCHEMA_UPDATE=1) rewrites the files.

use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema")
}

#[test]
fn r_proto_2_the_checked_in_schema_is_generated_from_the_types_and_current() {
    let update = std::env::var("KROWK_SCHEMA_UPDATE").is_ok_and(|v| !v.is_empty());
    let mut stale = Vec::new();
    for (name, want) in gruntas_harness::schema::files() {
        let path = dir().join(name);
        if update {
            std::fs::write(&path, &want).unwrap();
            continue;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
            stale.push(name);
        }
    }
    // No file in the directory that the generator does not write — save
    // `codex/`, the schema of the protocol krowk speaks to Codex, which is
    // Codex's own, pinned by scripts/codex_schema.sh.
    let generated: Vec<&str> = gruntas_harness::schema::files().iter().map(|(n, _)| *n).collect();
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name == "codex" {
            continue;
        }
        assert!(generated.contains(&name.as_str()), "schema/{name} is not generated from any type — remove it");
    }
    assert!(stale.is_empty(), "schema/{stale:?} no longer match the Rust types — run `make schema` and commit the result");
}
