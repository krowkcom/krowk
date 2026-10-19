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
//! - `user-keys.json`: the generations of the person's user key this
//!   device holds (`user_key::UserKeys`): the newest wrapped to this
//!   device's key (`UserKey::wrap_to`), with its generation and id, and the
//!   wrap of each older generation under the next. Every session key is
//!   sealed under these (engineering/devices.md → Keys). Written only by
//!   `save_user_keys`, which never goes back a generation.
//! - `device-list.json`: the signed device list as this device last
//!   verified it, entry 0 to its head, which is the pin; read back by
//!   verifying it again. Whose
//!   signatures a session record is checked against (`session_record`).
//!
//! - `signing.json`: the device's Ed25519 relay signing key
//!   (`e2e::SigningKey`), made the first time a relay is joined. It proves
//!   which device connects, and opens nothing.
//!
//! These files and the recovery phrase are the copies that last: the
//! registry holds keys only wrapped to devices.
//!
//! Where the five live is the platform's (R-CLIENT-4): `Keystore` makes
//! every rule above itself — the pin, a generation never going back, a wrap
//! checked against this device — and reaches storage only through
//! `KeyStorage`, one slot per file. `FileStorage` is krowk's home, as above;
//! the iPhone's Keychain plugs in the same way, and no platform writes a
//! rule again (R-CLIENT-1). `MemoryStorage` holds them in memory, for tests
//! and fakes.

use crate::e2e::{self, AccountKey, DeviceKey, KeyId, SigningKey};
use crate::device_chain::{Chain, SignedEntry};
use crate::user_key::{UserKey, UserKeyId, UserKeys};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// One of the five things a device keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    Device,
    Signing,
    Account,
    UserKeys,
    DeviceList,
}

impl Slot {
    /// Its file in krowk's home.
    pub fn file_name(self) -> &'static str {
        match self {
            Slot::Device => "device.json",
            Slot::Signing => "signing.json",
            Slot::Account => "account-key.json",
            Slot::UserKeys => "user-keys.json",
            Slot::DeviceList => "device-list.json",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Slot::Device => "device key",
            Slot::Signing => "signing key",
            Slot::Account => "account key",
            Slot::UserKeys => "user keys",
            Slot::DeviceList => "device list",
        }
    }
}

/// A slot's lock, held until dropped. What it holds is the storage's own.
pub struct StorageLock(#[allow(dead_code)] Box<dyn Send>);

impl StorageLock {
    pub fn new(guard: impl Send + 'static) -> StorageLock {
        StorageLock(Box::new(guard))
    }
}

/// Where a device's keys are kept: bytes in, bytes out, one slot at a time.
/// No rule lives here — what may be written is `Keystore`'s to decide.
/// Object-safe, so a platform implements it in its own language.
pub trait KeyStorage: Send + Sync {
    /// The slot's bytes; None when it is empty.
    fn read(&self, slot: Slot) -> Result<Option<Zeroizing<Vec<u8>>>, String>;
    /// Replaces the slot's bytes atomically: a reader sees the old or the new.
    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String>;
    /// Empties the slot; Ok when it was empty already.
    fn remove(&self, slot: Slot) -> Result<(), String>;
    /// The slot's lock: one writer at a time, until the guard is dropped.
    /// Not reentrant — `Keystore` never takes one while holding one.
    fn lock(&self, slot: Slot) -> Result<StorageLock, String>;
    /// How an error message names the slot.
    fn describe(&self, slot: Slot) -> String;
}

/// The keys as files in krowk's home: each `0600`, each replaced by rename
/// through krowk-api's credentials write. A slot's lock is a `.lock` file
/// beside its file: `account-key.lock`, `device-list.lock`.
#[derive(Debug, Clone)]
pub struct FileStorage {
    home: PathBuf,
}

impl FileStorage {
    pub fn new(home: &Path) -> FileStorage {
        FileStorage { home: home.to_path_buf() }
    }

    pub fn path(&self, slot: Slot) -> PathBuf {
        self.home.join(slot.file_name())
    }
}

impl KeyStorage for FileStorage {
    fn read(&self, slot: Slot) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        let path = self.path(slot);
        match std::fs::read(&path) {
            Ok(raw) => Ok(Some(Zeroizing::new(raw))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{} cannot be read: {e}", self.describe(slot))),
        }
    }

    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String> {
        krowk_api::home::make(&self.home)?;
        krowk_api::creds::write_bytes(&self.path(slot), bytes)
    }

    fn remove(&self, slot: Slot) -> Result<(), String> {
        let path = self.path(slot);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("could not remove {}: {e}", self.describe(slot))),
            _ => Ok(()),
        }
    }

    fn lock(&self, slot: Slot) -> Result<StorageLock, String> {
        krowk_api::home::make(&self.home)?;
        krowk_api::creds::lock(&self.path(slot)).map(StorageLock::new)
    }

    fn describe(&self, slot: Slot) -> String {
        self.path(slot).display().to_string()
    }
}

/// How long a `MemoryStorage` lock is waited for, as long as a file's is.
const MEMORY_LOCK_WAIT: Duration = Duration::from_secs(45);

type Held = Arc<(Mutex<HashSet<Slot>>, Condvar)>;

/// The keys in memory, wiped when replaced or dropped: for tests and fakes,
/// and the rules run on it exactly as on files. Its lock excludes across
/// threads as a file's does across processes.
#[derive(Default)]
pub struct MemoryStorage {
    slots: Mutex<HashMap<Slot, Zeroizing<Vec<u8>>>>,
    held: Held,
}

impl MemoryStorage {
    pub fn new() -> MemoryStorage {
        MemoryStorage::default()
    }
}

impl std::fmt::Debug for MemoryStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut full: Vec<Slot> = unpoisoned(&self.slots).keys().copied().collect();
        full.sort_by_key(|s| s.file_name());
        f.debug_struct("MemoryStorage").field("full", &full).finish_non_exhaustive()
    }
}

fn unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `MemoryStorage` slot's lock: frees the slot when dropped.
struct MemoryLock {
    held: Held,
    slot: Slot,
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        let (slots, freed) = &*self.held;
        unpoisoned(slots).remove(&self.slot);
        freed.notify_all();
    }
}

impl KeyStorage for MemoryStorage {
    fn read(&self, slot: Slot) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        Ok(unpoisoned(&self.slots).get(&slot).map(|b| Zeroizing::new(b.to_vec())))
    }

    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String> {
        unpoisoned(&self.slots).insert(slot, Zeroizing::new(bytes.to_vec()));
        Ok(())
    }

    fn remove(&self, slot: Slot) -> Result<(), String> {
        unpoisoned(&self.slots).remove(&slot);
        Ok(())
    }

    fn lock(&self, slot: Slot) -> Result<StorageLock, String> {
        let (slots, freed) = &*self.held;
        let until = Instant::now() + MEMORY_LOCK_WAIT;
        let mut held = unpoisoned(slots);
        while held.contains(&slot) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("another writer held {} for {} seconds — try again", self.describe(slot), MEMORY_LOCK_WAIT.as_secs()));
            }
            held = freed.wait_timeout(held, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        held.insert(slot);
        Ok(StorageLock::new(MemoryLock { held: self.held.clone(), slot }))
    }

    fn describe(&self, slot: Slot) -> String {
        format!("the in-memory {} slot", slot.noun())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct DeviceListFile {
    #[serde(default)]
    version: u8,
    /// Each entry as signed, from entry 0: its bytes and its signatures,
    /// hex. The last is the pin.
    #[serde(default)]
    entries: Vec<(String, String)>,
}

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

#[derive(Default, Serialize, Deserialize)]
struct UserKeysFile {
    #[serde(default)]
    version: u8,
    /// The device the newest generation is wrapped to, hex; checked against
    /// this device's key when the file is read, as the account file's is.
    #[serde(default)]
    device_id: String,
    /// The newest generation this device holds.
    #[serde(default)]
    generation: u32,
    /// Its id, hex: what the key must open to.
    #[serde(default)]
    key_id: String,
    /// `UserKey::wrap_to`'s blob for this device, hex.
    #[serde(default)]
    wrapped: String,
    /// `UserKey::wrap_previous`'s blob for each older generation, hex,
    /// oldest first.
    #[serde(default)]
    previous: Vec<String>,
}

/// One device's keys, and the rules they are kept by, over a `KeyStorage`.
#[derive(Clone)]
pub struct Keystore {
    storage: Arc<dyn KeyStorage>,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keystore").field("device", &self.storage.describe(Slot::Device)).finish_non_exhaustive()
    }
}

/// What `init` or `recover` did.
#[derive(Debug)]
pub struct Setup {
    pub device: DeviceKey,
    pub device_created: bool,
    pub account: AccountKey,
}

impl Keystore {
    /// The keys kept in `storage`.
    pub fn with(storage: Arc<dyn KeyStorage>) -> Keystore {
        Keystore { storage }
    }

    /// The keys kept as files in the krowk home `home`.
    pub fn new(home: &Path) -> Keystore {
        Keystore::with(Arc::new(FileStorage::new(home)))
    }

    /// How an error message names `slot`: the storage's words for it — a
    /// file's full path, so the terminal's "move it aside" says which file;
    /// the slot's name where there is no path.
    pub fn describe(&self, slot: Slot) -> String {
        self.storage.describe(slot)
    }

    /// Whether `slot` holds anything, valid or not; Err when it cannot be
    /// read.
    pub fn holds(&self, slot: Slot) -> Result<bool, String> {
        Ok(self.storage.read(slot)?.is_some())
    }

    fn load<T: DeserializeOwned>(&self, slot: Slot) -> Result<Option<T>, String> {
        load(&*self.storage, slot)
    }

    /// `v` as the slot's JSON, as the credentials write lays it out, from a
    /// buffer wiped after: the device and signing keys go through here.
    /// Sized so the device, signing and account files never reallocate and
    /// leave a copy behind; the user keys and the device list can outgrow
    /// it, and hold nothing that opens without the device key.
    fn store<T: Serialize>(&self, slot: Slot, v: &T) -> Result<(), String> {
        let mut buf = Zeroizing::new(Vec::with_capacity(1024));
        serde_json::to_writer_pretty(&mut *buf, v).expect("key file serializes");
        buf.push(b'\n');
        self.storage.write(slot, &buf)
    }

    /// This device's key, if it has one.
    pub fn device(&self) -> Result<Option<DeviceKey>, String> {
        let Some(f) = self.load::<DeviceFile>(Slot::Device)? else { return Ok(None) };
        let bad = || format!("{} does not hold a device key krowk reads — move it aside and run `krowk sync recover`", self.describe(Slot::Device));
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
    /// account slot's lock, so two krowks agree on one device key.
    pub fn device_key(&self) -> Result<DeviceKey, String> {
        let _lock = self.storage.lock(Slot::Account)?;
        Ok(self.device_or_create()?.0)
    }

    /// This device's key, made and stored when it has none. `true` when it
    /// was made now. The caller holds the account slot's lock.
    fn device_or_create(&self) -> Result<(DeviceKey, bool), String> {
        if let Some(d) = self.device()? {
            return Ok((d, false));
        }
        let d = DeviceKey::generate();
        self.store(Slot::Device, &DeviceFile { version: 1, secret: e2e::hex(&d.secret_bytes()[..]) })?;
        Ok((d, true))
    }

    /// This device's relay signing key, made and stored now when it has
    /// none, under the account slot's lock as the device key is. Its own
    /// slot rather than a field of the device key's, so a device made before
    /// it existed gains one without its device key being written again.
    pub fn signing_key(&self) -> Result<SigningKey, String> {
        let _lock = self.storage.lock(Slot::Account)?;
        if let Some(f) = self.load::<DeviceFile>(Slot::Signing)? {
            let bad = || format!("{} does not hold a signing key krowk reads — move it aside, and krowk makes a new one to register", self.describe(Slot::Signing));
            if f.version != 1 {
                return Err(bad());
            }
            let mut raw = e2e::unhex(&f.secret).ok_or_else(bad)?;
            let key = SigningKey::from_secret(&raw).map_err(|_| bad());
            raw.zeroize();
            return key;
        }
        let k = SigningKey::generate();
        self.store(Slot::Signing, &DeviceFile { version: 1, secret: e2e::hex(&k.secret_bytes()[..]) })?;
        Ok(k)
    }

    /// Whether this home holds an account key, and its id.
    pub fn account_id(&self) -> Result<Option<KeyId>, String> {
        let Some(f) = self.load::<AccountFile>(Slot::Account)? else { return Ok(None) };
        Ok(Some(self.parse_id(&f)?))
    }

    fn parse_id(&self, f: &AccountFile) -> Result<KeyId, String> {
        let bad = || format!("{} does not hold a wrapped account key krowk reads — move it aside and run `krowk sync recover`", self.describe(Slot::Account));
        if f.version != 1 {
            return Err(bad());
        }
        let id = e2e::unhex(&f.key_id).and_then(|b| <[u8; 16]>::try_from(b).ok()).ok_or_else(bad)?;
        Ok(KeyId(id))
    }

    /// The account key, unwrapped with this device's key.
    pub fn account(&self) -> Result<Option<AccountKey>, String> {
        let Some(f) = self.load::<AccountFile>(Slot::Account)? else { return Ok(None) };
        let name = self.describe(Slot::Account);
        let id = self.parse_id(&f)?;
        let device = self.device()?.ok_or_else(|| format!("{name} is here but this device's key ({}) is not, so it cannot be opened — run `krowk sync recover` with the recovery phrase", self.describe(Slot::Device)))?;
        if f.device_id != device.id().to_string() {
            return Err(format!("{name} was wrapped for device {}, not this device ({}) — run `krowk sync recover` with the recovery phrase", f.device_id, device.id()));
        }
        let blob = e2e::unhex(&f.wrapped).ok_or_else(|| format!("{name} is not valid"))?;
        e2e::unwrap_account_key(&blob, id, &device).map(Some).map_err(|e| format!("{name}: {e}"))
    }

    /// The account key, wrapped to this device (made now if it has no
    /// key). A different account key already here is replaced only when
    /// `recover` put it here too, and refused when `join` did.
    /// `replaced` names the key it replaced. What sets the account key up
    /// for the session sync that still runs on it, until that moves to the
    /// user key.
    pub fn recover(&self, account: AccountKey) -> Result<(Setup, Option<KeyId>), String> {
        let _lock = self.storage.lock(Slot::Account)?;
        let mut replaced = None;
        // The same key again keeps how it came here: a key `init` made stays
        // protected from a later `recover` of another phrase.
        let mut origin = "recover".to_string();
        if let Some(f) = self.load::<AccountFile>(Slot::Account)? {
            let id = self.parse_id(&f)?;
            if id == account.id() {
                origin = if f.origin == "recover" { f.origin } else { "init".into() };
            } else if f.origin != "recover" {
                return Err(format!("this home already holds account key {id}, put here by `krowk sync init` or `join` — recovering another into it would lose that one; use a fresh krowk home, or move {} aside", self.describe(Slot::Account)));
            } else {
                replaced = Some(id);
            }
        }
        let (device, device_created) = self.device_or_create()?;
        self.save(&device, &account, &origin)?;
        Ok((Setup { device, device_created, account }, replaced))
    }

    /// The generations of the user key this device holds, opened with its
    /// device key; None when it holds none yet.
    pub fn user_keys(&self) -> Result<Option<UserKeys>, String> {
        self.user_keys_in(&*self.storage)
    }

    /// The user keys in `other` rather than this keystore's storage, opened
    /// with this device's key: the old ones a start-over keeps aside.
    /// Errors name the slot as `other` describes it.
    pub fn user_keys_in(&self, other: &dyn KeyStorage) -> Result<Option<UserKeys>, String> {
        let Some(f) = load::<UserKeysFile>(other, Slot::UserKeys)? else { return Ok(None) };
        self.open_user_keys(&f, &other.describe(Slot::UserKeys)).map(Some)
    }

    /// `f`, opened with this device's key; `name` is where it was read.
    fn open_user_keys(&self, f: &UserKeysFile, name: &str) -> Result<UserKeys, String> {
        let bad = || format!("{name} does not hold user keys krowk reads — move it aside and add this device again");
        if f.version != 1 {
            return Err(bad());
        }
        let device = self.device()?.ok_or_else(|| format!("{name} is here but this device's key ({}) is not, so it cannot be opened — add this device again", self.describe(Slot::Device)))?;
        if f.device_id != device.id().to_string() {
            return Err(format!("{name} was wrapped for device {}, not this device ({}) — add this device again", f.device_id, device.id()));
        }
        let id = e2e::unhex(&f.key_id).and_then(|b| <[u8; 16]>::try_from(b).ok()).map(UserKeyId).ok_or_else(bad)?;
        let blob = e2e::unhex(&f.wrapped).ok_or_else(bad)?;
        let newest = UserKey::unwrap(&blob, f.generation, id, &device).map_err(|e| format!("{name}: {e}"))?;
        let previous = f.previous.iter().map(|w| e2e::unhex(w).ok_or_else(bad)).collect::<Result<Vec<_>, _>>()?;
        UserKeys::new(newest, previous).map_err(|e| format!("{name}: {e}"))
    }

    /// Keeps `keys` as the user keys this device holds, wrapped to its
    /// device key (made now if it has none), replacing the slot atomically.
    /// What the device already holds is never lost or swapped: `keys` must
    /// be the same newest generation, or a newer one that opens back down
    /// to exactly the one held (`UserKeys::adopt` checks a wrap the
    /// registry delivered the same way). The wraps of generations below the
    /// one held are the ones already kept; `keys` adds only the links from
    /// it up to its newest, and any the store lacked, so a set that omits
    /// or garbles an older wrap loses nothing. The caller has checked the
    /// newest key against the verified device chain.
    pub fn save_user_keys(&self, keys: &UserKeys) -> Result<(), String> {
        let _lock = self.storage.lock(Slot::Account)?;
        let new = keys.newest();
        let mut wraps: std::collections::BTreeMap<u32, Vec<u8>> = keys.wraps().map(|(g, w)| (g, w.to_vec())).collect();
        if let Some(held) = self.user_keys()? {
            let top = held.newest();
            if new.generation() < top.generation() {
                return Err(format!("this device holds user key generation {}, newer than generation {} — refused, so no generation is lost", top.generation(), new.generation()));
            }
            if keys.open(top.generation()).ok().as_ref() != Some(top) {
                return Err(format!("the user keys do not lead back to generation {} this device holds ({}) — refused", top.generation(), top.id()));
            }
            for (g, w) in held.wraps() {
                wraps.insert(g, w.to_vec());
            }
        }
        let (device, _) = self.device_or_create()?;
        let wrapped = new.wrap_to(&device.public()).map_err(|e| e.0)?;
        let file = UserKeysFile {
            version: 1,
            device_id: device.id().to_string(),
            generation: new.generation(),
            key_id: new.id().to_string(),
            wrapped: e2e::hex(&wrapped),
            previous: wraps.values().map(|w| e2e::hex(w)).collect(),
        };
        self.store(Slot::UserKeys, &file)
    }

    /// The device list as this device last verified it, verified again
    /// from entry 0; None when it has kept none. Its head is this device's
    /// pin: every sync command that opens or seals a session takes its
    /// signers and the current generation from here.
    pub fn device_list(&self) -> Result<Option<Chain>, String> {
        self.device_list_in(&*self.storage)
    }

    /// The device list in `other` rather than this keystore's storage,
    /// verified from entry 0: the old one a start-over keeps aside. Errors
    /// name the slot as `other` describes it.
    pub fn device_list_in(&self, other: &dyn KeyStorage) -> Result<Option<Chain>, String> {
        let Some(f) = load::<DeviceListFile>(other, Slot::DeviceList)? else { return Ok(None) };
        verify_list(&f, &other.describe(Slot::DeviceList)).map(Some)
    }

    /// Verifies `entries` against the list kept here — they must reach its
    /// head and hold it there, so a shorter, forked or other person's list
    /// is refused — and keeps them, replacing the slot atomically. The only
    /// store of the device list; its head is the pin.
    pub fn save_device_list(&self, entries: &[SignedEntry]) -> Result<Chain, String> {
        let _lock = self.storage.lock(Slot::DeviceList)?;
        let pin = match self.load::<DeviceListFile>(Slot::DeviceList)? {
            Some(f) => Some(verify_list(&f, &self.describe(Slot::DeviceList))?.head()),
            None => None,
        };
        let chain = Chain::verify(entries, pin).map_err(|e| e.0)?;
        let file = DeviceListFile { version: 1, entries: entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect() };
        self.store(Slot::DeviceList, &file)?;
        Ok(chain)
    }

    /// Forgets the user keys and the device list: a pairing that failed
    /// after they were kept leaves nothing behind. Only for a machine that is
    /// not on the list — `save_user_keys` never goes back a generation and
    /// `save_device_list` never moves the pin back, and this is not a way
    /// round either for a device that is.
    pub fn forget_user_keys(&self) -> Result<(), String> {
        let _lock = self.storage.lock(Slot::Account)?;
        for slot in [Slot::UserKeys, Slot::DeviceList] {
            self.storage.remove(slot)?;
        }
        Ok(())
    }

    fn save(&self, device: &DeviceKey, account: &AccountKey, origin: &str) -> Result<(), String> {
        let wrapped = e2e::wrap_account_key(account, &device.public()).map_err(|e| e.0)?;
        let file = AccountFile { version: 1, key_id: account.id().to_string(), device_id: device.id().to_string(), wrapped: e2e::hex(&wrapped), origin: origin.into() };
        self.store(Slot::Account, &file)
    }
}

/// `slot`'s JSON in `storage` as `T`, None when it is empty. A slot that is
/// not JSON is named with a line and column only, never serde's words,
/// which quote the value they could not take.
fn load<T: DeserializeOwned>(storage: &dyn KeyStorage, slot: Slot) -> Result<Option<T>, String> {
    let Some(raw) = storage.read(slot)? else { return Ok(None) };
    serde_json::from_slice(&raw).map(Some).map_err(|e| format!("{} is not valid (line {}, column {})", storage.describe(slot), e.line(), e.column()))
}

/// The device list `f` holds, verified from entry 0; `name` is where it was
/// read.
fn verify_list(f: &DeviceListFile, name: &str) -> Result<Chain, String> {
    let bad = || format!("{name} does not hold a device list krowk reads — move it aside and add this device to your devices again");
    if f.version != 1 || f.entries.is_empty() {
        return Err(bad());
    }
    let entries = f.entries.iter().map(|(e, s)| e2e::unhex(e).zip(e2e::unhex(s)).and_then(|(e, s)| SignedEntry::from_parts(e, &s).ok())).collect::<Option<Vec<_>>>().ok_or_else(bad)?;
    Chain::verify(&entries, None).map_err(|e| format!("{name}: {e}"))
}
