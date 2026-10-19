//! R-PROTO-2, R-FFI-1: the Swift the phone compiles is generated from this
//! crate's Rust, never written by hand, and it generates on Linux from the
//! host build on every `make check`. Library mode, as the crate doc's
//! manual command: UniFFI reads the interface out of the cdylib cargo built
//! beside this test, and the module names out of `uniffi.toml`.

use std::path::PathBuf;

/// The host build's cdylib: `target/<profile>/`, the parent of the `deps/`
/// this test binary runs from.
fn cdylib() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    let deps = exe.parent().expect("the test binary is in deps/");
    let dir = deps.parent().expect("deps/ is in target/<profile>/");
    let name = format!(
        "{}krowk_ffi{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    // deps/ first: the build that made this test binary always writes it
    // there, while target/<profile>/ holds whatever a plain `cargo build`
    // (the crate doc's manual command) last copied up, which may be older.
    [deps.join(&name), dir.join(&name)]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is built beside {}", deps.display()))
}

#[test]
fn r_proto_2_swift_bindings_generate_from_the_host_build() {
    let out = std::env::temp_dir().join(format!("krowk-ffi-swift-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let utf8 = |p: PathBuf| {
        p.into_os_string()
            .into_string()
            .expect("a UTF-8 path")
            .into()
    };
    uniffi::generate(uniffi::GenerateOptions {
        languages: vec![uniffi::TargetLanguage::Swift],
        source: utf8(cdylib()),
        out_dir: utf8(out.clone()),
        // swiftformat is not on Linux.
        format: false,
        // Only the workspace's manifests: krowk_ffi's uniffi.toml is all
        // the generator needs, and the test stays offline.
        metadata_no_deps: true,
        ..Default::default()
    })
    .expect("uniffi generates the Swift bindings");

    for file in ["Krowk.swift", "KrowkFFI.h", "KrowkFFI.modulemap"] {
        assert!(
            out.join(file).is_file(),
            "{file} is generated (module Krowk over KrowkFFI, uniffi.toml)"
        );
    }
    // Unformatted, UniFFI's templates space a declaration their own way
    // (`parsePairingCode(text: String)throws  -> String`), so whitespace is
    // left out of the comparison.
    let squeeze = |s: &str| s.split_whitespace().collect::<String>();
    let swift = squeeze(&std::fs::read_to_string(out.join("Krowk.swift")).expect("Krowk.swift reads"));
    for decl in ["func protocolVersion() -> UInt32", "func parsePairingCode(text: String) throws -> String", "enum KrowkError"] {
        assert!(swift.contains(&squeeze(decl)), "Krowk.swift declares `{decl}`");
    }
    let _ = std::fs::remove_dir_all(&out);
}
