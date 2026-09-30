//! One person's device list for a sync test: each krowk home the test
//! makes is put on it, and every home keeps the list as it now stands and
//! the user key it leaves current — what adding a device will leave in a
//! home once `krowk devices` does (D5/D6).

use krowk_client::device_chain::{Chain, Change, Kind, SignedEntry, Subject};
use krowk_client::e2e::SigningKey;
use krowk_client::keystore::Keystore;
use krowk_client::user_key::{UserKey, UserKeys};

/// When the list is made, by its devices' clocks.
const T0: u64 = 1_790_000_000;

#[derive(Default)]
pub struct People {
    entries: Vec<SignedEntry>,
    chain: Option<Chain>,
    user: Option<UserKey>,
    /// The first device's signing key, which adds the others.
    first: Option<SigningKey>,
    homes: Vec<Keystore>,
}

impl People {
    /// The home `ks` (whose device and signing keys exist) on the list as
    /// `name`, and every home on it given the list as it now stands.
    pub fn enlist(&mut self, ks: &Keystore, name: &str) {
        let (device, signing) = (ks.device().unwrap().unwrap(), ks.signing_key().unwrap());
        let subject = Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: device.public(), signing: signing.public() };
        match (&self.chain, &self.first) {
            (Some(chain), Some(first)) => {
                let (chain, batch) = chain.batch(self.user.as_ref().unwrap(), vec![Change::Add(subject)], chain.devices()[0].id(), first, T0 + self.entries.len() as u64).unwrap();
                self.entries.extend(batch.entries);
                self.chain = Some(chain);
            }
            _ => {
                let (chain, start) = Chain::start(subject, &signing, None, T0).unwrap();
                self.entries = start.entries;
                self.chain = Some(chain);
                self.user = Some(start.newest);
                self.first = Some(SigningKey::from_secret(&*signing.secret_bytes()).unwrap());
            }
        }
        self.homes.push(ks.clone());
        for home in &self.homes {
            home.save_device_list(&self.entries).unwrap();
            home.save_user_keys(&UserKeys::new(self.user.clone().unwrap(), []).unwrap()).unwrap();
        }
    }

    #[allow(dead_code)] // Not every test that shares this reads it.
    pub fn chain(&self) -> &Chain {
        self.chain.as_ref().expect("a device first")
    }

    #[allow(dead_code)]
    pub fn user(&self) -> &UserKey {
        self.user.as_ref().expect("a device first")
    }
}
