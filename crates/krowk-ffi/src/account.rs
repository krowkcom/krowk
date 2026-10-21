//! The phone, signed in: its krowk API key and its device keys, read from
//! the app's [`KeyStorage`] (R-FFI-3), and the registry client that signs
//! as this device. Pairing and the session list make their calls through
//! it. No export here returns a private key.

use std::sync::Arc;

use krowk_api::Client;
use krowk_client::e2e::{DeviceId, DeviceSigner};
use krowk_client::keystore::Keystore;

use crate::KrowkError;
use crate::sign_in::Credentials;
use crate::storage::{CREDENTIALS, Foreign, KeyStorage};

/// The signed-in phone, opened from the app's storage.
#[derive(uniffi::Object)]
pub struct Account {
    /// The registry's `/v1` URL, which the relay and its env are picked by.
    pub(crate) api: String,
    /// Signs every call as this device; the session exports call through it.
    pub(crate) client: Arc<Client>,
    pub(crate) keystore: Keystore,
    /// This device, which a session's relay ticket is asked for.
    pub(crate) device: DeviceId,
    storage: Arc<dyn KeyStorage>,
    workspace: String,
    workspace_name: String,
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
            api,
            client: Arc::new(client),
            keystore,
            device: device_id,
            storage,
            workspace: creds.workspace.clone(),
            workspace_name: creds.workspace_name.clone(),
        }))
    }

    /// This device's id, hex: what the device list and the approval of a
    /// new device name it by.
    pub fn device_id(&self) -> String {
        self.device.to_string()
    }

    /// The workspace the key belongs to; empty when the registry named none.
    pub fn workspace(&self) -> String {
        self.workspace.clone()
    }

    /// The workspace's name, as the registry's approval named it: what the
    /// phone says it is signed in to. Empty when the registry named none.
    pub fn workspace_name(&self) -> String {
        self.workspace_name.clone()
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
    let read = storage
        .read(CREDENTIALS.into())
        .map_err(|e| KrowkError::Internal { detail: format!("could not read the Keychain item {CREDENTIALS}: {e}") })?;
    let Some(raw) = read.map(krowk_client::Zeroizing::new) else {
        return Err(signed_out("this device is not signed in to krowk".into()));
    };
    match serde_json::from_slice::<Credentials>(&raw) {
        Ok(c) if c.version == 1 && !c.token.is_empty() => Ok(c),
        _ => Err(signed_out(format!("the Keychain item {CREDENTIALS} does not hold a key krowk reads — sign in again"))),
    }
}

pub(crate) fn internal(detail: String) -> KrowkError {
    KrowkError::Internal { detail }
}
