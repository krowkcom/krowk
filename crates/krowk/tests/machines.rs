//! `krowk machines` (R-ALLOC-2, R-ALLOC-12): the built binary on a scratch
//! home. Turning this machine's acceptance of handed-over tasks off and on
//! writes `sync.accept` in the person's own config, which the announcement
//! and the `run` command's admission read.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn home(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-machines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".krowk")).unwrap();
    root.canonicalize().unwrap()
}

fn krowk(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn config(home: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(home.join(".krowk/config.json")).unwrap()).unwrap()
}

/// Slice 09 (R-ALLOC-12, D14): `accept off` writes `sync.accept: false`
/// and `accept on` removes it, on being the default, keeping what else the
/// config says.
#[test]
fn alloc_09_accept_off_and_on_set_sync_accept() {
    let home = home("accept");
    std::fs::write(home.join(".krowk/config.json"), json!({"sync": {"idleMinutes": 5}}).to_string()).unwrap();
    let off = krowk(&home, &["machines", "accept", "off"]);
    assert!(off.status.success(), "{}", String::from_utf8_lossy(&off.stderr));
    assert_eq!(config(&home)["sync"], json!({"idleMinutes": 5, "accept": false}));
    let on = krowk(&home, &["machines", "accept", "on"]);
    assert!(on.status.success(), "{}", String::from_utf8_lossy(&on.stderr));
    assert_eq!(config(&home)["sync"], json!({"idleMinutes": 5}), "on is the default, so the key goes");
    let _ = std::fs::remove_dir_all(&home);
}
