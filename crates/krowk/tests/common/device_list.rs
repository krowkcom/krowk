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

    /// The list as it now stands, put in the registry at `api` for the
    /// person `token` names — as the first device's `sync init` would, a
    /// start-over each time so it can be called after every `enlist` — so a
    /// host that reads the registry's list finds this one.
    #[allow(dead_code)]
    pub fn publish(&self, api: &str, token: &str) {
        let first = &self.homes[0];
        let device = first.device().unwrap().unwrap();
        let user = self.user.as_ref().unwrap();
        let post = krowk_api::sync::ListPost {
            entries: self.entries.iter().map(|e| (krowk_client::e2e::hex(&e.bytes), krowk_client::e2e::hex(&e.signatures_bytes()))).collect(),
            links: Vec::new(),
            wraps: self.chain().devices().iter().map(|d| (d.id().to_string(), krowk_client::e2e::hex(&user.wrap_to(&d.device).unwrap()))).collect(),
            start_over: true,
        };
        // Another key of the same person (`tok#…`), so the machines' own
        // keys stay unbound.
        let other = format!("{}#people", token.split('#').next().unwrap());
        let signer = krowk_client::e2e::DeviceSigner::new(device.id(), first.signing_key().unwrap()).shared();
        krowk_api::Client::new(api, &other).signed_by(signer).init_device_list(&post).unwrap();
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
