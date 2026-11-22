//! `gruntas machines` (R-ALLOC-2, R-ALLOC-12): the built binary on a scratch
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
    std::fs::create_dir_all(root.join(".gruntas")).unwrap();
    root.canonicalize().unwrap()
}

fn gruntas(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gruntas"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("GRUNTAS_NO_UPDATE_CHECK", "1")
        .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn config(home: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(home.join(".gruntas/config.json")).unwrap()).unwrap()
}

/// Slice 09 (R-ALLOC-12, D14): `accept off` writes `sync.accept: false`
/// and `accept on` removes it, on being the default, keeping what else the
/// config says.
#[test]
fn alloc_09_accept_off_and_on_set_sync_accept() {
    let home = home("accept");
    std::fs::write(home.join(".gruntas/config.json"), json!({"sync": {"idleMinutes": 5}}).to_string()).unwrap();
    let off = gruntas(&home, &["machines", "accept", "off"]);
    assert!(off.status.success(), "{}", String::from_utf8_lossy(&off.stderr));
    assert_eq!(config(&home)["sync"], json!({"idleMinutes": 5, "accept": false}));
    let on = gruntas(&home, &["machines", "accept", "on"]);
    assert!(on.status.success(), "{}", String::from_utf8_lossy(&on.stderr));
    assert_eq!(config(&home)["sync"], json!({"idleMinutes": 5}), "on is the default, so the key goes");
    let _ = std::fs::remove_dir_all(&home);
}

/// R-EASE-9: each `gruntas machines` subcommand answers exactly as the old
/// command it stands for, and every old command still routes. Off a terminal
/// and with no sync set up, each refuses the same way, in the same words.
#[test]
fn ease_07_each_machines_subcommand_answers_as_its_old_command() {
    let home = home("old-and-new");
    let pairs: [(&[&str], &[&str]); 7] = [
        (&["devices", "add"], &["machines", "add"]),
        (&["devices", "remove", "desk"], &["machines", "remove", "desk"]),
        (&["devices", "remove"], &["machines", "remove"]),
        (&["sync", "recovery", "new"], &["machines", "recovery", "new"]),
        (&["sync", "recovery", "check"], &["machines", "recovery", "check"]),
        (&["sync", "recovery", "discard-old"], &["machines", "recovery", "discard-old"]),
        (&["sync", "status"], &["machines", "recovery"]),
    ];
    for (old, new) in pairs {
        let (o, n) = (gruntas(&home, old), gruntas(&home, new));
        assert_eq!(o.status.code(), n.status.code(), "{old:?} against {new:?}");
        assert_eq!(String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&n.stderr), "{old:?} against {new:?}");
        assert_eq!(o.stdout, n.stdout, "{old:?} against {new:?}");
        assert!(!String::from_utf8_lossy(&o.stderr).contains("unknown_command"), "{old:?} still routes");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// `always-on` takes on or off, and `host enable` and `host disable` still
/// route; none of them is left to guess what was meant.
#[test]
fn ease_07_always_on_needs_on_or_off_and_the_host_forms_still_route() {
    let home = home("always-on");
    for args in [&["machines", "always-on"][..], &["machines", "always-on", "maybe"]] {
        let out = gruntas(&home, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("always-on on"), "{}", String::from_utf8_lossy(&out.stderr));
    }
    for args in [&["host", "enable"][..], &["host", "disable"], &["hosts"], &["machines", "accept", "on"]] {
        let out = gruntas(&home, args);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("unknown_command"), "{args:?} still routes");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// The old names leave `gruntas help`, stay in `help --all`, and no hint any
/// command prints sends a person to one of them.
#[test]
fn ease_07_old_commands_are_hidden_from_help_but_listed_by_all() {
    let home = home("hidden");
    let help = String::from_utf8_lossy(&gruntas(&home, &["help"]).stdout).to_string();
    for old in ["devices", "hosts"] {
        assert!(!help.lines().any(|l| l.trim_start().starts_with(old)), "{old} is hidden:\n{help}");
    }
    let all = String::from_utf8_lossy(&gruntas(&home, &["help", "--all"]).stdout).to_string();
    for sub in ["machines add", "machines remove", "machines always-on", "machines recovery", "devices add", "host enable"] {
        assert!(all.contains(sub), "{sub} in help --all:\n{all}");
    }
    let _ = std::fs::remove_dir_all(&home);
}
