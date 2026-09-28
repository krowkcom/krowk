//! `krowk sync init` and `krowk sync recover`, the built binary on scratch
//! homes (R-E2E-3, R-E2E-4): first setup at a terminal shows a phrase and
//! keeps the account key only once it is typed back; a second home given
//! the phrase holds the identical account key; the phrase is in no file.

#![cfg(all(feature = "harness", unix))]

#[path = "common/pty.rs"]
mod pty;

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-sync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["laptop", "desktop"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    root.canonicalize().unwrap()
}

/// krowk with `home` as HOME and nothing else of this machine's.
fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
        .current_dir(home)
        .stdin(Stdio::null());
    c
}

fn piped(home: &Path, args: &[&str], input: &str) -> Output {
    let mut child = command(home, args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

/// The 24 numbered words `init` printed.
fn phrase_in(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<&str> = tokens.windows(2).filter(|w| w[0].strip_suffix('.').is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)))).map(|w| w[1]).take(24).collect();
    assert_eq!(words.len(), 24, "{text}");
    words.join(" ")
}

fn files_holding(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            found.extend(files_holding(&p, needle));
        } else if std::fs::read(&p).is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle)) {
            found.push(p);
        }
    }
    found
}

#[test]
fn r_e2e_4_sync_init_needs_a_terminal() {
    let r = root("notty");
    let out = command(&r.join("laptop"), &["sync", "init", "--json"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("confirmation_required") && err.contains("needs a person at a terminal"), "{err}");
    assert!(!r.join("laptop/.krowk/account-key.json").exists());
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn r_e2e_4_the_phrase_from_sync_init_restores_the_account_key_on_a_fresh_home() {
    let r = root("recover");
    let wait = Duration::from_secs(20);

    let mut c = command(&r.join("laptop"), &["sync", "init", "--json"]);
    c.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(c, 120, 30);
    assert!(t.wait_for("Type the phrase back", wait).is_some(), "{}", t.text());
    let phrase = phrase_in(&t.text());
    // A wrong word first: asked again, nothing kept yet.
    let wrong = phrase.replacen(phrase.split(' ').next().unwrap(), "zzzz", 1);
    t.write(format!("{wrong}\r").as_bytes());
    assert!(t.wait_for("is not one a recovery phrase uses", wait).is_some(), "{}", t.text());
    assert!(!r.join("laptop/.krowk/account-key.json").exists());
    std::thread::sleep(Duration::from_millis(300));
    t.write(format!("{phrase}\r").as_bytes());
    let exit = t.wait(wait).expect("krowk sync init finished");
    assert!(exit.success(), "{}", t.text());
    let text = t.text();
    // The result, pretty-printed on the terminal after the prompts.
    let json_at = text.rfind("{\r\n  \"ok\"").unwrap_or_else(|| panic!("{text}"));
    let made: Value = serde_json::Deserializer::from_str(&text[json_at..].replace("\r\n", "\n")).into_iter::<Value>().next().unwrap().unwrap();
    let account = made["data"]["account_key"].as_str().unwrap().to_string();
    assert_eq!(made["data"]["device_created"], true, "{made}");
    // Typed back, it was never echoed: the words appear once, as printed.
    assert_eq!(text.matches(&phrase.split(' ').take(3).collect::<Vec<_>>().join(" ")).count(), 0, "the typed phrase was echoed");

    // The fresh home, from the words alone.
    let out = piped(&r.join("desktop"), &["sync", "recover", "--json"], &format!("{phrase}\n"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let restored: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(restored["data"]["account_key"], account.as_str(), "the identical account key");
    assert_eq!(restored["data"]["recovered"], true);
    assert_ne!(restored["data"]["device"], made["data"]["device"], "a new home is a new device");

    // `init` showed the key id beside the words, to compare with `recover`'s.
    assert!(text.contains(&format!("Key id {account}")), "{text}");

    // A valid phrase for another key (a typo that passed the checksum) is
    // restored, and the right one entered next replaces it.
    let retry = r.join("retry");
    std::fs::create_dir_all(&retry).unwrap();
    let other_key = format!("{} art", ["abandon"; 23].join(" "));
    let out = piped(&retry, &["sync", "recover", "--json"], &other_key);
    let wrong_id = serde_json::from_slice::<Value>(&out.stdout).unwrap()["data"]["account_key"].clone();
    assert_ne!(wrong_id, account.as_str());
    let out = piped(&retry, &["sync", "recover", "--json"], &phrase);
    let fixed: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!((&fixed["data"]["account_key"], &fixed["data"]["replaced"]), (&Value::from(account.as_str()), &wrong_id));

    // For a person, the result is its sentence, not the JSON envelope.
    let third = r.join("third");
    std::fs::create_dir_all(&third).unwrap();
    let out = piped(&third, &["sync", "recover", "--format", "human"], &phrase);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.starts_with(&format!("account key {account} restored")) && !said.contains('{'), "{said}");

    // A phrase with a word changed is refused, and nothing is written.
    let other = r.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let out = piped(&other, &["sync", "recover", "--json"], &wrong);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad_recovery_phrase"));
    assert!(!other.join(".krowk/account-key.json").exists());

    // The phrase is in no file krowk wrote, on either home.
    assert_eq!(files_holding(&r, &phrase), Vec::<PathBuf>::new());
    for home in ["laptop", "desktop"] {
        use std::os::unix::fs::PermissionsExt;
        for f in ["device.json", "account-key.json"] {
            let mode = std::fs::metadata(r.join(home).join(".krowk").join(f)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{home}/{f}");
        }
    }
    let _ = std::fs::remove_dir_all(&r);
}
