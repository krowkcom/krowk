//! The recovery kit on the built binary, on scratch homes and with no
//! registry to reach (D6): what `krowk sync init`, `recover` and `recovery
//! check` refuse or decide before anything leaves the machine.

#![cfg(all(feature = "harness", unix))]

use krowk_client::device_chain::{Chain, Kind, Subject};
use krowk_client::keystore::Keystore;
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::UserKeys;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-kit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

/// krowk with `home` as HOME and nothing else of this machine's, and a
/// registry nobody answers at.
fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
        .env("KROWK_TOKEN", "krowk_sk_kit")
        .current_dir(home)
        .stdin(Stdio::null());
    c
}

/// With the debug build's stand-in for the person, and `input` piped in.
fn piped(home: &Path, args: &[&str], input: &str) -> Output {
    let mut c = command(home, args);
    c.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL", "1");
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

/// `home` set up as `sync init` leaves it: its device first on a list,
/// with `kit` as the recovery device when there is one, the list kept and
/// generation 1 held.
fn set_up(home: &Path, kit: Option<&RecoveryKit>) {
    let ks = Keystore::new(&home.join(".krowk"));
    let (device, signing) = (ks.device_key().unwrap(), ks.signing_key().unwrap());
    let me = Subject { kind: Kind::Device, name: "laptop".into(), os: "linux".into(), device: device.public(), signing: signing.public() };
    let kd = kit.map(RecoveryKit::device);
    let recovery = kd.as_ref().map(|d| (Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: d.key.public(), signing: d.signing.public() }, &d.signing));
    let (_, start) = Chain::start(me, &signing, recovery, 1_790_000_000).unwrap();
    ks.save_device_list(&start.entries).unwrap();
    ks.save_user_keys(&UserKeys::new(start.newest, []).unwrap()).unwrap();
}

fn err(out: &Output) -> String {
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// D6: init shows the kit and asks for a fresh sign-in, so off a terminal
/// it refuses before either, and nothing is written.
#[test]
fn d6_sync_init_needs_a_person_at_a_terminal() {
    let r = root("notty");
    let e = err(&command(&r, &["sync", "init", "--json"]).output().unwrap());
    assert!(e.contains("confirmation_required") && e.contains("a person at a terminal"), "{e}");
    assert!(!r.join(".krowk/device-list.json").exists() && !r.join(".krowk/user-keys.json").exists());
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: the kit is 12 words. The 24-word phrase is gone, and a word typed
/// wrong is refused with which, before anything is asked of the registry.
#[test]
fn d6_recover_refuses_words_that_are_not_a_kit_before_anything_leaves_the_machine() {
    let r = root("words");
    let kit = RecoveryKit::generate();
    let words = kit.words().to_string();
    let first = words.split(' ').next().unwrap().to_string();
    for (input, why) in [
        (format!("{words} {words}"), "12 words, and this is 24"),
        (words.replacen(&first, "zzzz", 1), "word 1 is not one a recovery kit uses"),
    ] {
        let e = err(&piped(&r, &["sync", "recover", "--json"], &input));
        assert!(e.contains("bad_recovery_kit") && e.contains(why), "{e}");
        assert!(!e.contains(&words[..12]), "the words are never echoed: {e}");
    }
    assert!(!r.join(".krowk/device-list.json").exists());
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: `recovery check` tests the words against the list as this device
/// last verified it, with no registry to ask.
#[test]
fn d6_recovery_check_tests_the_words_locally() {
    let r = root("check");
    let (kit, other) = (RecoveryKit::generate(), RecoveryKit::generate());
    set_up(&r, Some(&kit));

    let out = piped(&r, &["sync", "recovery", "check", "--json"], &kit.words());
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["matches"], true, "{v}");

    let e = err(&piped(&r, &["sync", "recovery", "check", "--json"], &other.words()));
    assert!(e.contains("kit_not_on_list"), "{e}");

    // And a list with no kit says so, rather than that the words are wrong.
    let bare = root("check-bare");
    set_up(&bare, None);
    let r = bare;
    let e = err(&piped(&r, &["sync", "recovery", "check", "--json"], &kit.words()));
    assert!(e.contains("no recovery kit") && e.contains("krowk sync recovery new"), "{e}");
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn d6_status_on_a_machine_not_set_up_says_how_to_set_it_up() {
    let r = root("status");
    let e = err(&command(&r, &["sync", "status", "--json"]).output().unwrap());
    assert!(e.contains("not_set_up") && e.contains("krowk sync init") && e.contains("krowk sync recover"), "{e}");
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn d6_the_kits_flags_belong_to_the_commands_that_make_one() {
    let r = root("flags");
    let e = err(&command(&r, &["sync", "status", "--save", "kit.txt"]).output().unwrap());
    assert!(e.contains("`--save` is only a flag of `krowk sync init` and `krowk sync recovery new`"), "{e}");
    let e = err(&command(&r, &["sync", "recover", "--start-over"]).output().unwrap());
    assert!(e.contains("`--start-over` is only a flag of `krowk sync init`"), "{e}");
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: a device on a device list hosts or attaches to nothing — and so
/// seals nothing new — until it has verified the list from its pin. With
/// the registry out of reach it refuses, rather than seal under a
/// generation a removed device may hold.
#[test]
fn d6_a_device_that_cannot_verify_its_list_seals_nothing_new() {
    let r = root("seal");
    set_up(&r, None);
    let e = err(&command(&r, &["sync", "attach", "0190f3a8-7c1e-7a9b-8c2d-3e4f5a6b7c8d", "--json"]).output().unwrap());
    assert!(e.contains("network") || e.contains("unreachable"), "{e}");
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: a second `sync init` on a machine already set up is refused before
/// the sign-in, which would replace its key with one that speaks for no
/// device.
#[test]
fn d6_init_on_a_machine_already_set_up_is_refused_before_the_sign_in() {
    let r = root("again");
    set_up(&r, None);
    let e = err(&piped(&r, &["sync", "init", "--json"], ""));
    assert!(e.contains("already_set_up") && e.contains("--start-over"), "{e}");
    let _ = std::fs::remove_dir_all(&r);
}

/// D6: with the registry out of reach, `status` says what this device last
/// verified — no kit, here — and that it was not checked.
#[test]
fn d6_status_offline_says_what_was_last_verified() {
    let r = root("offline");
    set_up(&r, None);
    let out = command(&r, &["sync", "status", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((v["data"]["checked"].clone(), v["data"]["recovery_kit"].clone()), (false.into(), false.into()), "{v}");
    assert!(v["summary"].as_str().unwrap().contains("no recovery kit"), "{v}");
    let _ = std::fs::remove_dir_all(&r);
}
