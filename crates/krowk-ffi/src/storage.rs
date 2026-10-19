//! Where the phone keeps its keys and its krowk API key (R-FFI-3): a
//! [`KeyStorage`] the app implements over the Keychain, and [`Foreign`],
//! which hands it to krowk-client's keystore as its storage seam. Every
//! rule about what may be written stays the keystore's (R-CLIENT-1); Swift
//! only stores bytes.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use krowk_client::Zeroizing;
use krowk_client::keystore::{self, Slot, StorageLock};

use crate::KrowkError;

/// The storage item the krowk API key is kept in, beside the keystore's
/// five: the token, its key id and its workspace, as one JSON blob.
pub(crate) const CREDENTIALS: &str = "credentials.json";

/// Named items of bytes, kept by the app: on the iPhone, Keychain items.
/// Each `write` replaces an item whole, so a reader sees the old bytes or
/// the new. The names are krowk's file names (`device.json`,
/// `credentials.json`, …), so a Keychain item is called what the file is
/// called on a computer.
#[uniffi::export(with_foreign)]
pub trait KeyStorage: Send + Sync {
    /// The item's bytes; None when there is no such item.
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError>;
    /// Replaces the item's bytes, creating it when absent.
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError>;
    /// Removes the item; Ok when there was none.
    fn remove(&self, slot: String) -> Result<(), KrowkError>;
}

/// The app's storage as the keystore reaches it. Bytes read are wiped when
/// the keystore is done with them; bytes written are copied once into the
/// buffer UniFFI hands Swift, which frees it unwiped — the Keychain write
/// is the one copy Swift is asked to make.
pub(crate) struct Foreign(pub(crate) Arc<dyn KeyStorage>);

impl Foreign {
    /// What the app said, with which item it was saying it about.
    fn failed(&self, what: &str, slot: Slot, e: KrowkError) -> String {
        format!("could not {what} {}: {e}", keystore::KeyStorage::describe(self, slot))
    }
}

/// How long a slot's lock is waited for, as long as a file's is.
const LOCK_WAIT: Duration = Duration::from_secs(45);

/// The slot names locked now, in this process. The Keychain replaces an
/// item atomically but has no lock to hold across a read and a write, and
/// in M1 the app is one process, so an in-process lock is the whole of
/// it — one table for every adapter, so two `Account`s opened at once
/// still agree on one device key. The Notification Service Extension (M2)
/// makes a second process that shares the Keychain, and needs a lock
/// both can see.
static HELD: LazyLock<(Mutex<HashSet<&'static str>>, Condvar)> = LazyLock::new(Default::default);

fn held() -> MutexGuard<'static, HashSet<&'static str>> {
    HELD.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A slot's lock: frees the slot when dropped.
struct Held(&'static str);

impl Drop for Held {
    fn drop(&mut self) {
        held().remove(self.0);
        HELD.1.notify_all();
    }
}

impl keystore::KeyStorage for Foreign {
    fn read(&self, slot: Slot) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        let read = self.0.read(slot.file_name().into()).map_err(|e| self.failed("read", slot, e))?;
        Ok(read.map(Zeroizing::new))
    }

    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String> {
        self.0.write(slot.file_name().into(), bytes.to_vec()).map_err(|e| self.failed("write", slot, e))
    }

    fn remove(&self, slot: Slot) -> Result<(), String> {
        self.0.remove(slot.file_name().into()).map_err(|e| self.failed("remove", slot, e))
    }

    fn lock(&self, slot: Slot) -> Result<StorageLock, String> {
        let name = slot.file_name();
        let until = Instant::now() + LOCK_WAIT;
        let mut locked = held();
        while locked.contains(name) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("another writer held {} for {} seconds — try again", self.describe(slot), LOCK_WAIT.as_secs()));
            }
            locked = HELD.1.wait_timeout(locked, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        locked.insert(name);
        Ok(StorageLock::new(Held(name)))
    }

    fn describe(&self, slot: Slot) -> String {
        format!("the Keychain item {}", slot.file_name())
    }
}
