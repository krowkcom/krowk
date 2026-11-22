// The version is stamped from KROWK_VERSION at build time; a change to it has
// to rebuild, which Cargo does not do for an environment variable on its own.
fn main() {
    println!("cargo:rerun-if-env-changed=KROWK_VERSION");
}
