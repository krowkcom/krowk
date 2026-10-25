//! The phone, signed in: its krowk API key and its device keys, read from
//! the app's [`KeyStorage`] (R-FFI-3), and the registry client that signs
//! as this device. Pairing and the session list make their calls through
//! it. No export here returns a private key.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use krowk_api::Client;
use krowk_client::device_chain::Kind;
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
    /// The join under way, by its cancel flag (`join_pairing`): one at a
    /// time.
    pub(crate) pairing: Mutex<Option<Arc<AtomicBool>>>,
    /// The starts this account sealed lately, by command id, so a retry of
    /// one sends the very bytes again (`start_session`).
    pub(crate) starts: Mutex<crate::hosts::Sealed>,
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
            pairing: Mutex::new(None),
            starts: Mutex::default(),
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

    /// The person's devices as the device list this phone holds says (its
    /// pin, kept by the join): no registry call. Empty until it is paired.
    /// Recovery kits are not devices and are left out.
    pub fn devices(&self) -> Result<Vec<ListedDevice>, KrowkError> {
        let Some(chain) = self.keystore.device_list().map_err(internal)? else { return Ok(Vec::new()) };
        Ok(chain
            .devices()
            .iter()
            .filter(|d| d.kind == Kind::Device)
            .map(|d| ListedDevice { id: d.id().to_string(), name: d.name.clone(), os: d.os.clone(), this_device: d.id() == self.device })
            .collect())
    }

    /// Registers this device's APNs token with the registry, so the
    /// person's other devices can ring it (R-IOS-12): `token_hex` as the
    /// app read it (64 to 200 hex digits), `environment` `sandbox` or
    /// `production`, the APNs host the token belongs to. Replaces any
    /// token this device had; a token another device held moves here.
    /// Signed by this device. Blocking (a registry call, retried): never
    /// on the main thread. The token is sent in the body only, and never
    /// logged or kept.
    pub fn set_push_token(&self, token_hex: String, environment: String) -> Result<(), KrowkError> {
        Ok(self.client.put_push_token(&token_hex, &environment)?)
    }

    /// This device takes no more pushes: its token, if any, is dropped at
    /// the registry. Ok when it had none. Signed by this device. Blocking
    /// (a registry call, retried): never on the main thread.
    pub fn clear_push_token(&self) -> Result<(), KrowkError> {
        Ok(self.client.delete_push_token()?)
    }

    /// Forgets the krowk API key. The device keys stay, so signing in again
    /// is the same device.
    pub fn sign_out(&self) -> Result<(), KrowkError> {
        self.storage.remove(CREDENTIALS.into())
    }
}

/// A device on the person's device list: its id (32 lowercase hex), the
/// name and os it was added with, and whether it is this phone.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ListedDevice {
    pub id: String,
    pub name: String,
    pub os: String,
    pub this_device: bool,
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
