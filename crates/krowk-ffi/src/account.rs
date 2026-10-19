//! The phone, signed in: its krowk API key and its device keys, read from
//! the app's [`KeyStorage`] (R-FFI-3), and the registry client that signs
//! as this device. Pairing and the session list make their calls through
//! it. No export here returns a private key.

use std::sync::Arc;

use krowk_api::Client;
use krowk_client::e2e::DeviceSigner;
use krowk_client::keystore::Keystore;

use crate::KrowkError;
use crate::sign_in::Credentials;
use crate::storage::{CREDENTIALS, Foreign, KeyStorage};

/// The signed-in phone, opened from the app's storage.
#[derive(uniffi::Object)]
pub struct Account {
    // Held for the pairing and session exports, which call through them.
    #[allow(dead_code)]
    client: Arc<Client>,
    keystore: Keystore,
    storage: Arc<dyn KeyStorage>,
    device_id: String,
    workspace: String,
}

#[uniffi::export]
impl Account {
    /// The account kept in `storage`, against the registry at `api` (its
    /// `/v1` URL). Refused with `signed_out` when `storage` holds no key.
    /// Makes the device's keys on first use, so every later open is the same
    /// device; blocking the first time, while they are made and written.
    #[uniffi::constructor]
    pub fn open(api: String, storage: Arc<dyn KeyStorage>) -> Result<Arc<Account>, KrowkError> {
        let creds = credentials(&*storage)?;
        let keystore = Keystore::with(Arc::new(Foreign(Arc::clone(&storage))));
        let device = keystore.device_key().map_err(internal)?;
        let signing = keystore.signing_key().map_err(internal)?;
        let device_id = device.id();
        drop(device);
        let client = Client::new(&api, &creds.token).signed_by(DeviceSigner::new(device_id, signing).shared());
        Ok(Arc::new(Account {
            client: Arc::new(client),
            keystore,
            storage,
            device_id: device_id.to_string(),
            workspace: creds.workspace.clone(),
        }))
    }

    /// This device's id, hex: what the device list and the approval of a
    /// new device name it by.
    pub fn device_id(&self) -> String {
        self.device_id.clone()
    }

    /// The workspace the key belongs to; empty when the registry named none.
    pub fn workspace(&self) -> String {
        self.workspace.clone()
    }

    /// Whether this device holds the person's user keys yet: false until
    /// it is paired.
    pub fn has_user_keys(&self) -> Result<bool, KrowkError> {
        Ok(self.keystore.user_keys().map_err(internal)?.is_some())
    }

    /// Forgets the krowk API key. The device keys stay, so signing in again
    /// is the same device.
    pub fn sign_out(&self) -> Result<(), KrowkError> {
        self.storage.remove(CREDENTIALS.into())
    }
}

/// The key kept in `storage`; `signed_out` when there is none, or none
/// krowk can read.
fn credentials(storage: &dyn KeyStorage) -> Result<Credentials, KrowkError> {
    let signed_out = |detail: String| KrowkError::Refused { code: "signed_out".into(), detail };
    let Some(raw) = storage.read(CREDENTIALS.into())?.map(krowk_client::Zeroizing::new) else {
        return Err(signed_out("this device is not signed in to krowk".into()));
    };
    match serde_json::from_slice::<Credentials>(&raw) {
        Ok(c) if c.version == 1 && !c.token.is_empty() => Ok(c),
        _ => Err(signed_out(format!("the Keychain item {CREDENTIALS} does not hold a key krowk reads — sign in again"))),
    }
}

fn internal(detail: String) -> KrowkError {
    KrowkError::Internal { detail }
}
