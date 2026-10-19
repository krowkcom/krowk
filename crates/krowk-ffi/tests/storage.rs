//! R-FFI-3, R-CLIENT-1: the phone's keys live in a `KeyStorage` the app
//! implements, under krowk's file names, and the keystore's rules run on it
//! unchanged: an account opens only with a key stored, makes the device's
//! keys once, and signing out forgets the key but not the device.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use krowk_ffi::{Account, KeyStorage, KrowkError};

/// The app's storage, as a Swift Keychain class would keep it.
#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

impl KeyStorage for Memory {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        Ok(self.0.lock().unwrap().get(&slot).cloned())
    }
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError> {
        self.0.lock().unwrap().insert(slot, bytes);
        Ok(())
    }
    fn remove(&self, slot: String) -> Result<(), KrowkError> {
        self.0.lock().unwrap().remove(&slot);
        Ok(())
    }
}

impl Memory {
    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.0.lock().unwrap().keys().cloned().collect();
        n.sort();
        n
    }
}

/// A Keychain that refuses every write.
struct Locked;

impl KeyStorage for Locked {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        Ok((slot == "credentials.json").then(signed_in))
    }
    fn write(&self, slot: String, _: Vec<u8>) -> Result<(), KrowkError> {
        Err(KrowkError::Internal { detail: format!("errSecInteractionNotAllowed writing {slot}") })
    }
    fn remove(&self, _: String) -> Result<(), KrowkError> {
        Ok(())
    }
}

fn signed_in() -> Vec<u8> {
    br#"{"version":1,"token":"krowk_sk_test","key_id":"key_1","workspace":"ws_1","workspace_name":"Test"}"#.to_vec()
}

const API: &str = "http://127.0.0.1:9/v1";

fn refused(r: Result<Arc<Account>, KrowkError>) -> (String, String) {
    match r {
        Err(KrowkError::Refused { code, detail }) => (code, detail),
        Err(e) => panic!("not a refusal: {e:?}"),
        Ok(_) => panic!("opened"),
    }
}

#[test]
fn r_ffi_3_an_account_with_no_key_stored_is_signed_out() {
    let storage = Arc::new(Memory::default());
    let (code, _) = refused(Account::open(API.into(), storage.clone()));
    assert_eq!(code, "signed_out");
    // Nothing was made for a phone that is not signed in.
    assert!(storage.names().is_empty(), "{:?}", storage.names());
}

#[test]
fn r_ffi_3_an_unreadable_key_is_signed_out_too() {
    let storage = Arc::new(Memory::default());
    storage.write("credentials.json".into(), b"{\"version\":1}".to_vec()).unwrap();
    let (code, detail) = refused(Account::open(API.into(), storage));
    assert_eq!(code, "signed_out");
    assert!(detail.contains("credentials.json"), "{detail}");
}

#[test]
fn r_ffi_3_r_client_1_the_device_keys_are_made_once_under_krowks_file_names() {
    let storage = Arc::new(Memory::default());
    storage.write("credentials.json".into(), signed_in()).unwrap();
    let first = Account::open(API.into(), storage.clone()).unwrap();
    assert_eq!(first.workspace(), "ws_1");
    assert_eq!(first.device_id().len(), 32, "{}", first.device_id());
    assert_eq!(storage.names(), ["credentials.json", "device.json", "signing.json"]);
    let device = storage.read("device.json".into()).unwrap();

    let second = Account::open(API.into(), storage.clone()).unwrap();
    assert_eq!(second.device_id(), first.device_id());
    assert_eq!(storage.read("device.json".into()).unwrap(), device, "the device key was not made again");
    assert!(!second.has_user_keys().unwrap(), "not paired yet");
}

#[test]
fn r_ffi_3_signing_out_forgets_the_key_and_keeps_the_device() {
    let storage = Arc::new(Memory::default());
    storage.write("credentials.json".into(), signed_in()).unwrap();
    let account = Account::open(API.into(), storage.clone()).unwrap();
    account.sign_out().unwrap();
    assert_eq!(storage.names(), ["device.json", "signing.json"]);
    assert_eq!(refused(Account::open(API.into(), storage.clone())).0, "signed_out");

    storage.write("credentials.json".into(), signed_in()).unwrap();
    assert_eq!(Account::open(API.into(), storage).unwrap().device_id(), account.device_id());
}

#[test]
fn r_ffi_3_two_accounts_opened_at_once_agree_on_one_device() {
    let storage = Arc::new(Memory::default());
    storage.write("credentials.json".into(), signed_in()).unwrap();
    let opened: Vec<String> = (0..8)
        .map(|_| {
            let s = storage.clone();
            std::thread::spawn(move || Account::open(API.into(), s).unwrap().device_id())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    assert!(opened.iter().all(|d| *d == opened[0]), "{opened:?}");
}

#[test]
fn r_ffi_3_a_storage_error_names_the_keychain_item_and_says_what_swift_said() {
    let e = Account::open(API.into(), Arc::new(Locked)).err().expect("a write that fails");
    let KrowkError::Internal { detail } = e else { panic!("{e:?}") };
    assert!(detail.contains("the Keychain item device.json") && detail.contains("errSecInteractionNotAllowed"), "{detail}");
}
