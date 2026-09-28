//! This device's keys in krowk's home: two files, each `0600`, each
//! replaced by rename through krowk-api's credentials write, never written
//! in place.
//!
//! - `device.json`: the device's X25519 private key. Made on first use and
//!   never leaves the machine; a new home is a new device (R-E2E-3).
//! - `account-key.json`: the account key, wrapped to this device's key
//!   (`e2e::wrap_account_key`), with the account key's id. Useless without
//!   `device.json`, and `device.json` is useless without it: a copy of one
//!   file is not the account key.
//!
//! Until the registry holds wrapped keys (ticket 16), these are the only
//! copies of the account key there are, besides the recovery phrase.

use crate::e2e::{self, AccountKey, DeviceKey, KeyId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const DEVICE_FILE: &str = "device.json";
pub const ACCOUNT_FILE: &str = "account-key.json";

#[derive(Default, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct DeviceFile {
    #[serde(default)]
    version: u8,
    /// The X25519 private key, hex.
    #[serde(default)]
    secret: String,
}

#[derive(Default, Serialize, Deserialize)]
struct AccountFile {
    #[serde(default)]
    version: u8,
    /// The account key's id, hex: which key the phrase must restore.
    #[serde(default)]
    key_id: String,
    /// The device it is wrapped to, hex; a check, not a secret.
    #[serde(default)]
    device_id: String,
    /// `e2e::wrap_account_key`'s blob, hex.
    #[serde(default)]
    wrapped: String,
}

/// The files of one krowk home.
#[derive(Debug, Clone)]
pub struct Keystore {
    home: PathBuf,
}

/// What `init` or `recover` did.
#[derive(Debug)]
pub struct Setup {
    pub device: DeviceKey,
    pub device_created: bool,
    pub account: AccountKey,
}

impl Keystore {
    pub fn new(home: &Path) -> Keystore {
        Keystore { home: home.to_path_buf() }
    }

    pub fn device_path(&self) -> PathBuf {
        self.home.join(DEVICE_FILE)
    }

    pub fn account_path(&self) -> PathBuf {
        self.home.join(ACCOUNT_FILE)
    }

    /// This device's key, if it has one.
    pub fn device(&self) -> Result<Option<DeviceKey>, String> {
        let path = self.device_path();
        if !path.exists() {
            return Ok(None);
        }
        let f: DeviceFile = krowk_api::creds::read(&path)?;
        let bad = || format!("{} does not hold a device key krowk reads — move it aside and run `krowk sync recover`", path.display());
        if f.version != 1 {
            return Err(bad());
        }
        let mut raw = e2e::unhex(&f.secret).ok_or_else(bad)?;
        let key = DeviceKey::from_secret(&raw).map_err(|_| bad());
        raw.zeroize();
        key.map(Some)
    }

    /// This device's key, made and stored when it has none. `true` when it
    /// was made now. Under the account file's lock, so two krowks setting up
    /// at once agree on one device key.
    fn device_or_create(&self) -> Result<(DeviceKey, bool), String> {
        if let Some(d) = self.device()? {
            return Ok((d, false));
        }
        let d = DeviceKey::generate();
        let file = DeviceFile { version: 1, secret: e2e::hex(&d.secret_bytes()[..]) };
        krowk_api::creds::write(&self.device_path(), &file)?;
        Ok((d, true))
    }

    /// Whether this home holds an account key, and its id.
    pub fn account_id(&self) -> Result<Option<KeyId>, String> {
        let path = self.account_path();
        if !path.exists() {
            return Ok(None);
        }
        let f: AccountFile = krowk_api::creds::read(&path)?;
        Ok(Some(self.parse_id(&f)?))
    }

    fn parse_id(&self, f: &AccountFile) -> Result<KeyId, String> {
        let bad = || format!("{} does not hold a wrapped account key krowk reads — move it aside and run `krowk sync recover`", self.account_path().display());
        if f.version != 1 {
            return Err(bad());
        }
        let id = e2e::unhex(&f.key_id).and_then(|b| <[u8; 16]>::try_from(b).ok()).ok_or_else(bad)?;
        Ok(KeyId(id))
    }

    /// The account key, unwrapped with this device's key.
    pub fn account(&self) -> Result<Option<AccountKey>, String> {
        let path = self.account_path();
        if !path.exists() {
            return Ok(None);
        }
        let f: AccountFile = krowk_api::creds::read(&path)?;
        let id = self.parse_id(&f)?;
        let device = self.device()?.ok_or_else(|| format!("{} is here but this device's key ({}) is not, so it cannot be opened — run `krowk sync recover` with the recovery phrase", path.display(), self.device_path().display()))?;
        let blob = e2e::unhex(&f.wrapped).ok_or_else(|| format!("{} is not valid", path.display()))?;
        e2e::unwrap_account_key(&blob, id, &device).map(Some).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// First sync setup: a device key if there is none, and a new account
    /// key wrapped to it. `confirm` is shown the key (to show its phrase and
    /// have it entered again) before anything about the account is written;
    /// when it refuses, nothing is kept but the device key.
    pub fn init(&self, confirm: impl FnOnce(&AccountKey) -> Result<(), String>) -> Result<Setup, String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        if let Some(id) = self.account_id()? {
            return Err(format!("this home already holds account key {id} — sync is set up here; `krowk sync recover` restores a different one"));
        }
        let (device, device_created) = self.device_or_create()?;
        let account = AccountKey::generate();
        confirm(&account)?;
        self.save(&device, &account)?;
        Ok(Setup { device, device_created, account })
    }

    /// A fresh machine: the account key from its recovery phrase, wrapped
    /// to this device (made now if it has no key). Refused when this home
    /// already holds a different account key; the same one is a no-op.
    pub fn recover(&self, account: AccountKey) -> Result<Setup, String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        if let Some(id) = self.account_id()?
            && id != account.id()
        {
            return Err(format!("this home already holds another account key ({id}) — recovering into it would lose that one; use a fresh krowk home, or move {} aside", self.account_path().display()));
        }
        let (device, device_created) = self.device_or_create()?;
        self.save(&device, &account)?;
        Ok(Setup { device, device_created, account })
    }

    fn save(&self, device: &DeviceKey, account: &AccountKey) -> Result<(), String> {
        let wrapped = e2e::wrap_account_key(account, &device.public()).map_err(|e| e.0)?;
        let file = AccountFile { version: 1, key_id: account.id().to_string(), device_id: device.id().to_string(), wrapped: e2e::hex(&wrapped) };
        krowk_api::creds::write(&self.account_path(), &file)
    }
}
