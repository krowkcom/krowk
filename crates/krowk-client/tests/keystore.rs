//! The device key and the wrapped account key in a krowk home, and the
//! recovery phrase carrying the account key to a fresh one (R-E2E-3,
//! R-E2E-4). Each test gets homes of its own under a scratch directory.

use krowk_client::e2e::{self, AccountKey, SessionKey};
use krowk_client::keystore::Keystore;
use krowk_client::phrase;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-client-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn r_e2e_4_a_recovery_phrase_restores_the_account_key_on_a_fresh_home() {
    let root = scratch("recover");
    let (first, second) = (Keystore::new(&root.join("laptop")), Keystore::new(&root.join("desktop")));

    // First sync setup on the laptop: the phrase is shown and entered again.
    let mut shown = None;
    let setup = first
        .init(|k| {
            shown = Some(phrase::encode(k));
            Ok(())
        })
        .unwrap();
    assert!(setup.device_created);
    let words = shown.unwrap();
    assert_eq!(first.account().unwrap().unwrap(), setup.account);
    // A session key wrapped there, as sync would upload it.
    let session = *b"session-00000001";
    let key = SessionKey::generate();
    let wrapped = e2e::wrap_session_key(&key, &session, &setup.account);

    // A fresh home, with nothing but the words.
    assert!(second.account().unwrap().is_none());
    let (restored, replaced) = second.recover(phrase::decode(&words).unwrap()).unwrap();
    assert_eq!(replaced, None);
    assert!(restored.device_created);
    assert_ne!(restored.device.id(), setup.device.id(), "the second home is its own device");
    assert_eq!(restored.account, setup.account, "the identical account key");
    let reread = second.account().unwrap().unwrap();
    assert_eq!(e2e::unwrap_session_key(&wrapped, &session, &reread).unwrap().as_bytes(), key.as_bytes());

    // Each home's wrapped key opens with its own device key only.
    std::fs::copy(first.account_path(), second.account_path()).unwrap();
    assert!(second.account().unwrap_err().contains("was wrapped for device"));
    // And with the stored device id edited to match, the HPKE binding
    // still refuses it.
    let mut edited: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(second.account_path()).unwrap()).unwrap();
    edited["device_id"] = restored.device.id().to_string().into();
    std::fs::write(second.account_path(), edited.to_string()).unwrap();
    assert!(second.account().unwrap_err().contains("does not open"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn r_e2e_4_setup_writes_no_account_key_until_the_phrase_is_confirmed() {
    let root = scratch("confirm");
    let store = Keystore::new(&root.join("home"));
    let refused = store.init(|_| Err("the words did not match".into())).unwrap_err();
    assert_eq!(refused, "the words did not match");
    assert!(!store.account_path().exists());
    assert!(store.device().unwrap().is_some(), "the device key is kept: it names this machine, not the account");
    // Asked again, the same device key is used.
    let device = store.device().unwrap().unwrap().id();
    let setup = store.init(|_| Ok(())).unwrap();
    assert!(!setup.device_created);
    assert_eq!(setup.device.id(), device);
    // And a second setup is refused rather than replacing the key.
    assert!(store.init(|_| Ok(())).unwrap_err().contains("already holds account key"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn r_e2e_3_recovering_another_account_into_a_home_is_refused() {
    let root = scratch("other");
    let store = Keystore::new(&root.join("home"));
    let setup = store.init(|_| Ok(())).unwrap();
    assert!(store.recover(AccountKey::generate()).unwrap_err().contains("put here by `krowk sync init`"));
    // The same account again is fine, and changes nothing that matters —
    // including that `init` made it: another phrase is still refused after.
    store.recover(setup.account.clone()).unwrap();
    assert_eq!(store.account().unwrap().unwrap(), setup.account);
    assert!(store.recover(AccountKey::generate()).unwrap_err().contains("put here by `krowk sync init`"));
    assert_eq!(store.account().unwrap().unwrap(), setup.account);
    let _ = std::fs::remove_dir_all(&root);
}

/// A phrase mistyped into a valid one (1 in 256) restores the wrong key;
/// entering the right phrase next replaces it, since a phrase put it there.
#[test]
fn r_e2e_4_a_recover_is_retried_with_the_right_phrase() {
    let root = scratch("retry");
    let store = Keystore::new(&root.join("home"));
    let (wrong, right) = (AccountKey::generate(), AccountKey::generate());
    let (first, _) = store.recover(wrong.clone()).unwrap();
    let (second, replaced) = store.recover(right.clone()).unwrap();
    assert_eq!(replaced, Some(wrong.id()));
    assert_eq!(second.device.id(), first.device.id());
    assert_eq!(store.account().unwrap().unwrap(), right);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn r_e2e_3_the_key_files_are_0600_and_hold_no_plain_account_key() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("mode");
    let store = Keystore::new(&root.join("home"));
    let setup = store.init(|_| Ok(())).unwrap();
    for path in [store.device_path(), store.account_path()] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
    let account = std::fs::read_to_string(store.account_path()).unwrap();
    assert!(!account.contains(&e2e::hex(setup.account.as_bytes())));
    assert!(!account.contains(&*phrase::encode(&setup.account)));
    let _ = std::fs::remove_dir_all(&root);
}

/// R-RELAY-1: the relay signing key is made once, `0600`, beside the
/// device key, and read back as the same key.
#[cfg(unix)]
#[test]
fn r_relay_1_the_signing_key_is_made_once_and_kept_0600() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("signing");
    let store = Keystore::new(&root.join("home"));
    let first = store.signing_key().unwrap();
    assert_eq!(std::fs::metadata(store.signing_path()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(store.signing_key().unwrap().public(), first.public());
    std::fs::write(store.signing_path(), "{\"version\":2,\"secret\":\"00\"}").unwrap();
    assert!(store.signing_key().unwrap_err().contains("move it aside"));
    let _ = std::fs::remove_dir_all(&root);
}
