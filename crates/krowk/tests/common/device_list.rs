//! One person's device list for a sync test: each krowk home the test
//! makes is put on it, and every home keeps the list as it now stands and
//! the user key it leaves current — what adding a device will leave in a
//! home once `krowk devices` does (D5/D6). Published, the registry holds the
//! list too, and each home has a key of its own that speaks for its device
//! (`key`), as `sync init` and `sync join` leave a machine.

use krowk_client::device_chain::{Chain, Change, Kind, SignedEntry, Subject};
use krowk_client::e2e::SigningKey;
use krowk_client::keystore::Keystore;
use krowk_client::user_key::{UserKey, UserKeys};

/// When the list is made, by its devices' clocks.
const T0: u64 = 1_790_000_000;

#[allow(dead_code)] // Not every test that shares this makes a person.
#[derive(Default)]
pub struct People {
    entries: Vec<SignedEntry>,
    chain: Option<Chain>,
    user: Option<UserKey>,
    /// The first device's signing key, which adds the others.
    first: Option<SigningKey>,
    homes: Vec<(Keystore, String)>,
    /// How many entries, and how many homes' keys, the registry has.
    published: (usize, usize),
}

/// The key of the person `token` names that the home called `name` holds:
/// another key of the same person (`tok#name`), one per machine.
#[allow(dead_code)]
pub fn key(token: &str, name: &str) -> String {
    format!("{}#{name}", token.split('#').next().unwrap())
}

impl People {
    /// The home `ks` (whose device and signing keys exist) on the list as
    /// `name`, and every home on it given the list as it now stands.
    #[allow(dead_code)]
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
        self.homes.push((ks.clone(), name.into()));
        for (home, _) in &self.homes {
            home.save_device_list(&self.entries).unwrap();
            home.save_user_keys(&UserKeys::new(self.user.clone().unwrap(), []).unwrap()).unwrap();
        }
    }

    /// The list as it now stands, put in the registry at `api` for the
    /// person `token` names, and each home's own key (`key`) bound to its
    /// device — so it can be called after every `enlist`. The first time, as
    /// the first device's `sync init` posts it, from that home's key, which
    /// the init binds; after that the entries since, appended by the first
    /// device. Then each home not yet bound claims its device with its key,
    /// signed by that device, as `sync join` leaves a new machine.
    #[allow(dead_code)]
    pub fn publish(&mut self, api: &str, token: &str) {
        let (entries, homes) = self.published;
        let user = self.user.as_ref().unwrap();
        // Every device on a list it starts; only those added since on an
        // append, which rotates nothing.
        let wrapped = self.chain().devices().iter().skip(if entries == 0 { 0 } else { homes });
        let post = krowk_api::sync::ListPost {
            entries: self.entries[entries..].iter().map(|e| (krowk_client::e2e::hex(&e.bytes), krowk_client::e2e::hex(&e.signatures_bytes()))).collect(),
            links: Vec::new(),
            wraps: wrapped.map(|d| (d.id().to_string(), krowk_client::e2e::hex(&user.wrap_to(&d.device).unwrap()))).collect(),
            start_over: false,
        };
        let first = self.client(api, token, 0);
        if entries == 0 {
            first.init_device_list(&post).unwrap();
        } else if entries < self.entries.len() {
            first.append_device_list(&post).unwrap();
        }
        for i in homes.max(1)..self.homes.len() {
            self.client(api, token, i).claim_key_device().unwrap();
        }
        self.published = (self.entries.len(), self.homes.len());
    }

    /// How many homes are on the list.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.homes.len()
    }

    /// The registry at `api` as home `i`, with its own key, signed by its
    /// device.
    #[allow(dead_code)]
    pub fn client(&self, api: &str, token: &str, i: usize) -> krowk_api::Client {
        let (home, name) = &self.homes[i];
        let signer = krowk_client::e2e::DeviceSigner::new(home.device().unwrap().unwrap().id(), home.signing_key().unwrap()).shared();
        krowk_api::Client::new(api, &key(token, name)).signed_by(signer)
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

/// The home `ks` alone on a new device list in the registry at `api`, as its
/// `sync init` would post it, from the key `token`, which the post binds to
/// it — and nothing in the home changed. The registry as that device.
#[allow(dead_code)]
pub fn start(api: &str, token: &str, ks: &Keystore, name: &str) -> krowk_api::Client {
    let (device, signing) = (ks.device().unwrap().unwrap(), ks.signing_key().unwrap());
    let subject = Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: device.public(), signing: signing.public() };
    let (_, start) = Chain::start(subject, &signing, None, T0).unwrap();
    let post = krowk_api::sync::ListPost {
        entries: start.entries.iter().map(|e| (krowk_client::e2e::hex(&e.bytes), krowk_client::e2e::hex(&e.signatures_bytes()))).collect(),
        links: Vec::new(),
        wraps: vec![(device.id().to_string(), krowk_client::e2e::hex(&start.newest.wrap_to(&device.public()).unwrap()))],
        start_over: false,
    };
    let client = krowk_api::Client::new(api, token).signed_by(krowk_client::e2e::DeviceSigner::new(device.id(), signing).shared());
    client.init_device_list(&post).unwrap();
    client
}
