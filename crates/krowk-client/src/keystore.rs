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
//! - `signing.json`: the device's Ed25519 relay signing key
//!   (`e2e::SigningKey`), made the first time a relay is joined. It proves
//!   which device connects, and opens nothing.
//!
//! The registry holds the account key wrapped to a device only while an
//! approval carries it to that device (`join`); once collected it lives
//! here, so these files and the recovery phrase are the copies that last.

use crate::e2e::{self, AccountKey, DeviceKey, KeyId, SigningKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const DEVICE_FILE: &str = "device.json";
pub const ACCOUNT_FILE: &str = "account-key.json";
pub const SIGNING_FILE: &str = "signing.json";

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
    /// The device it is wrapped to, hex; a check, not a secret, compared
    /// with this device's key when the file is read.
    #[serde(default)]
    device_id: String,
    /// `init`, `recover` or `join`: how the key came to this home. A key a phrase
    /// restored may be replaced by another phrase (the first was mistyped
    /// into a valid one); a key `init` made may not, since this file and
    /// the phrase written down may be its only copies.
    #[serde(default)]
    origin: String,
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

    /// This device's key, made and stored now when it has none — what a new
    /// device shows the id of while it waits to be approved. Under the
    /// account file's lock, so two krowks agree on one device key.
    pub fn device_key(&self) -> Result<DeviceKey, String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        Ok(self.device_or_create()?.0)
    }

    /// This device's key, made and stored when it has none. `true` when it
    /// was made now. The caller holds the account file's lock.
    fn device_or_create(&self) -> Result<(DeviceKey, bool), String> {
        if let Some(d) = self.device()? {
            return Ok((d, false));
        }
        let d = DeviceKey::generate();
        let file = DeviceFile { version: 1, secret: e2e::hex(&d.secret_bytes()[..]) };
        krowk_api::creds::write(&self.device_path(), &file)?;
        Ok((d, true))
    }

    pub fn signing_path(&self) -> PathBuf {
        self.home.join(SIGNING_FILE)
    }

    /// This device's relay signing key, made and stored now when it has
    /// none, under the account file's lock as the device key is. Its own
    /// file rather than a field of `device.json`, so a device made before
    /// it existed gains one without its device key being written again.
    pub fn signing_key(&self) -> Result<SigningKey, String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        let path = self.signing_path();
        if path.exists() {
            let f: DeviceFile = krowk_api::creds::read(&path)?;
            let bad = || format!("{} does not hold a signing key krowk reads — move it aside, and krowk makes a new one to register", path.display());
            if f.version != 1 {
                return Err(bad());
            }
            let mut raw = e2e::unhex(&f.secret).ok_or_else(bad)?;
            let key = SigningKey::from_secret(&raw).map_err(|_| bad());
            raw.zeroize();
            return key;
        }
        let k = SigningKey::generate();
        krowk_api::creds::write(&path, &DeviceFile { version: 1, secret: e2e::hex(&k.secret_bytes()[..]) })?;
        Ok(k)
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
        if f.device_id != device.id().to_string() {
            return Err(format!("{} was wrapped for device {}, not this device ({}) — run `krowk sync recover` with the recovery phrase", path.display(), f.device_id, device.id()));
        }
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
        self.save(&device, &account, "init")?;
        Ok(Setup { device, device_created, account })
    }

    /// A fresh machine: the account key from its recovery phrase, wrapped
    /// to this device (made now if it has no key). A different account key
    /// already here is replaced only when a phrase put it here too — the
    /// retry after a word typed wrong — and refused when `init` made it.
    /// `replaced` names the key a retry replaced.
    pub fn recover(&self, account: AccountKey) -> Result<(Setup, Option<KeyId>), String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        let mut replaced = None;
        // The same key again keeps how it came here: a key `init` made stays
        // protected from a later `recover` of another phrase.
        let mut origin = "recover".to_string();
        if let Some(id) = self.account_id()? {
            let f: AccountFile = krowk_api::creds::read(&self.account_path())?;
            if id == account.id() {
                origin = if f.origin == "recover" { f.origin } else { "init".into() };
            } else if f.origin != "recover" {
                return Err(format!("this home already holds account key {id}, put here by `krowk sync init` or `join` — recovering another into it would lose that one; use a fresh krowk home, or move {} aside", self.account_path().display()));
            } else {
                replaced = Some(id);
            }
        }
        let (device, device_created) = self.device_or_create()?;
        self.save(&device, &account, &origin)?;
        Ok((Setup { device, device_created, account }, replaced))
    }

    /// A device another one approved: the account key it wrapped to this
    /// device (`e2e::wrap_account_key`'s blob), opened only as the key whose
    /// id the person was shown on the approving device. That id is the whole
    /// of the check against a registry that wrapped a key of its own choosing
    /// to this device: HPKE's Base mode does not say who sealed a blob, and
    /// `unwrap_account_key` refuses one that is not for `expected`.
    ///
    /// A home that already holds an account key keeps it unless it is the
    /// same key: joining is for a machine that has none.
    pub fn join(&self, wrapped: &[u8], expected: KeyId) -> Result<Setup, String> {
        krowk_api::home::make(&self.home)?;
        let _lock = krowk_api::creds::lock(&self.account_path())?;
        if let Some(id) = self.account_id()?
            && id != expected
        {
            return Err(format!("this home already holds account key {id} — joining would replace it; use a fresh krowk home, or move {} aside", self.account_path().display()));
        }
        let (device, device_created) = self.device_or_create()?;
        let account = e2e::unwrap_account_key(wrapped, expected, &device).map_err(|e| {
            format!("the approved key did not open as account key {expected} on this device ({e}) — check the id was typed as the approving device showed it; if it was, do not trust this approval, and ask again")
        })?;
        self.save(&device, &account, "join")?;
        Ok(Setup { device, device_created, account })
    }

    fn save(&self, device: &DeviceKey, account: &AccountKey, origin: &str) -> Result<(), String> {
        let wrapped = e2e::wrap_account_key(account, &device.public()).map_err(|e| e.0)?;
        let file = AccountFile { version: 1, key_id: account.id().to_string(), device_id: device.id().to_string(), wrapped: e2e::hex(&wrapped), origin: origin.into() };
        krowk_api::creds::write(&self.account_path(), &file)
    }
}
