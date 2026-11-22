//! A test's own directory, for a daemon to put its socket under.
#![allow(dead_code)]

use std::path::PathBuf;

/// `/tmp/krowk-<name>-<pid>`, empty. Under /tmp rather than the temp dir:
/// macOS's per-user TMPDIR is already ~50 bytes, and with a test's own
/// directory and `run/krowk/<home>/host.sock` on top the socket path runs
/// past the 103 bytes `sun_path` holds, which the daemon refuses.
pub fn root(name: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("krowk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
