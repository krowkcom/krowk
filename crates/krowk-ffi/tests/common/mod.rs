//! What the tests that need a paired phone and a host writing a session
//! at rest share: the stand-in registry, the phone's Keychain in memory,
//! and a host on the device list that writes through the harness's own
//! writer.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use krowk_api::Client;
use krowk_client::Zeroizing;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::e2e::{self, DeviceKey, DeviceSigner, SessionKey, SessionKeys, SigningKey};
use krowk_client::keystore::{self, Keystore, Slot, StorageLock};
use krowk_client::protocol::LogBody;
use krowk_client::session_record;
use krowk_client::user_key::UserKeys;
use krowk_ffi::{Account, KeyStorage, KrowkError};
use krowk_harness::sync::store::{HostedOn, Index, Writer};
use serde_json::{Value, json};

pub const TOKEN: &str = "krowk_sk_ffi_sessions_00000000000000000";
/// When the list is made, by its devices' clocks.
pub const T0: u64 = 1_790_000_000;

/// The phone's Keychain, in memory.
#[derive(Default)]
pub struct Memory(pub Mutex<HashMap<String, Vec<u8>>>);

impl KeyStorage for Memory {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        Ok(self.0.lock().unwrap().get(&slot).cloned())
    }
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError> {
        self.0.lock().unwrap().insert(slot, bytes);
        Ok(())
    }
    fn remove(&self, slot: String) -> Result<(), KrowkError> {
        self.0.lock().unwrap().remove(&slot);
        Ok(())
    }
}

/// The same Keychain as krowk-client's keystore reaches it, so the test
/// can put the phone on the device list as pairing would: its device keys,
/// the list and the user keys, in the items `Account` reads.
pub struct Bridge(pub Arc<Memory>);

impl keystore::KeyStorage for Bridge {
    fn read(&self, slot: Slot) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        Ok(self.0.0.lock().unwrap().get(slot.file_name()).cloned().map(Zeroizing::new))
    }
    fn write(&self, slot: Slot, bytes: &[u8]) -> Result<(), String> {
        self.0.0.lock().unwrap().insert(slot.file_name().into(), bytes.to_vec());
        Ok(())
    }
    fn remove(&self, slot: Slot) -> Result<(), String> {
        self.0.0.lock().unwrap().remove(slot.file_name());
        Ok(())
    }
    fn lock(&self, _: Slot) -> Result<StorageLock, String> {
        Ok(StorageLock::new(()))
    }
    fn describe(&self, slot: Slot) -> String {
        slot.file_name().into()
    }
}

/// Another key of the person `TOKEN` names, one per device (the stand-in's
/// `tok#name`).
pub fn key(name: &str) -> String {
    format!("{TOKEN}#{name}")
}

/// A phone's storage holding the key `token`, as sign-in leaves it.
pub fn signed_in(token: &str) -> Arc<Memory> {
    let m = Memory::default();
    let creds = json!({"version": 1, "token": token, "key_id": "key_phone", "workspace": "ws", "workspace_name": "Workspace"});
    m.write("credentials.json".into(), serde_json::to_vec(&creds).unwrap()).unwrap();
    Arc::new(m)
}

pub fn subject(name: &str, device: &DeviceKey, signing: &SigningKey) -> Subject {
    Subject { kind: Kind::Device, name: name.into(), os: "test".into(), device: device.public(), signing: signing.public() }
}

pub fn post(entries: &[krowk_client::device_chain::SignedEntry], wraps: Vec<(String, String)>) -> krowk_api::sync::ListPost {
    krowk_api::sync::ListPost { entries: entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(), links: Vec::new(), wraps, start_over: false }
}

/// A host on a new device list, the phone added to it, both bound to keys
/// of their own in the registry, and the phone's storage holding what
/// pairing leaves: its device keys, the list and the user keys.
pub struct World {
    pub registry: krowk_devregistry::Running,
    pub api: String,
    pub host: Arc<Client>,
    pub host_device: DeviceKey,
    pub host_signing: SigningKey,
    pub user: krowk_client::user_key::UserKey,
    pub phone: Arc<Account>,
    /// The phone's Keychain, for another `Account` over it.
    pub storage: Arc<Memory>,
}

/// A session the host holds the lease of and writes.
pub struct Hosted {
    pub writer: Writer,
    pub token: String,
    pub keys: SessionKeys,
    pub wrapped: String,
}

impl World {
    pub fn new() -> World {
        let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let api = format!("{}/v1", registry.url());

        let (host_device, host_signing) = (DeviceKey::generate(), SigningKey::generate());
        let (chain, start) = Chain::start(subject("host", &host_device, &host_signing), &host_signing, None, T0).unwrap();
        let user = start.newest;
        let host = Client::new(&api, &key("host")).signed_by(DeviceSigner::new(host_device.id(), SigningKey::from_secret(&*host_signing.secret_bytes()).unwrap()).shared());
        host.init_device_list(&post(&start.entries, vec![(host_device.id().to_string(), e2e::hex(&user.wrap_to(&host_device.public()).unwrap()))])).unwrap();

        // The phone makes its keys, as `Account::open` would, and is added.
        let storage = signed_in(&key("phone"));
        let ks = Keystore::with(Arc::new(Bridge(storage.clone())));
        let (phone_device, phone_signing) = (ks.device_key().unwrap(), ks.signing_key().unwrap());
        let (chain, batch) = chain.batch(&user, vec![Change::Add(subject("phone", &phone_device, &phone_signing))], host_device.id(), &host_signing, T0 + 1).unwrap();
        host.append_device_list(&post(&batch.entries, vec![(phone_device.id().to_string(), e2e::hex(&user.wrap_to(&phone_device.public()).unwrap()))])).unwrap();
        let phone_client = Client::new(&api, &key("phone")).signed_by(DeviceSigner::new(phone_device.id(), phone_signing).shared());
        phone_client.claim_key_device().unwrap();
        let entries: Vec<_> = start.entries.into_iter().chain(batch.entries).collect();
        assert_eq!(ks.save_device_list(&entries).unwrap().devices().len(), chain.devices().len());
        ks.save_user_keys(&UserKeys::new(user.clone(), []).unwrap()).unwrap();

        let phone = Account::open(api.clone(), storage.clone()).unwrap();
        assert_eq!(phone.device_id(), phone_device.id().to_string());
        World { registry, api, host: Arc::new(host), host_device, host_signing, user, phone, storage }
    }

    /// A session record for `id` under a new session key, signed by
    /// `signer` (its id, its key), with the index titled `title`; the key.
    pub fn record(&self, id: &str, title: &str, signer: (e2e::DeviceId, &SigningKey)) -> (SessionKey, String, Index) {
        let raw = krowk_client::sync::uuid(id);
        let key = SessionKey::generate();
        let sealed_key = e2e::wrap_session_key(&key, &raw, &self.user);
        let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, &self.user, signer.1).unwrap();
        let wrapped = e2e::hex(&sealed_key);
        let host = Some(HostedOn { device: self.host_device.id().to_string(), tailnet: None });
        let index = Index { title: title.into(), cwd: "/src/app".into(), host, ..Default::default() };
        let sealed_index = e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()));
        self.host.put_sync_session(id, &wrapped, Some((&e2e::hex(&signature), &signer.0.to_string())), Some(&sealed_index), None).unwrap();
        (key, wrapped, index)
    }

    /// The host takes session `id` up and writes `first` (chunk 0), then a
    /// checkpoint (chunk 1), then `then` (chunk 2), holding the lease.
    pub fn host_session(&self, id: &str, title: &str, first: &[Value], then: &[Value]) -> Hosted {
        let (key, wrapped, index) = self.record(id, title, (self.host_device.id(), &self.host_signing));
        let keys = SessionKeys::from(key);
        let lease = self.host.acquire_lease(id, &self.host_device.id().to_string(), 60, "development").unwrap();
        let mut w = Writer::take_up(self.host.clone(), keys.clone(), id, wrapped.clone(), index, lease.fence).unwrap();
        for e in first {
            w.push(e.clone());
        }
        w.flush(&lease.token).unwrap();
        w.checkpoint(None, &lease.token).unwrap();
        for e in then {
            w.push(e.clone());
        }
        w.flush(&lease.token).unwrap();
        Hosted { writer: w, token: lease.token, keys, wrapped }
    }
}

impl Hosted {
    pub fn write(&mut self, events: &[Value]) {
        for e in events {
            self.writer.push(e.clone());
        }
        self.writer.flush(&self.token).unwrap();
    }
}

/// A log event the phone decodes: a run opened, named `run`.
pub fn event(session: &str, n: u32, run: &str) -> Value {
    json!({"id": format!("01a0ec7b-2222-7000-8000-{n:012}"), "sessionId": session, "timeMs": n, "type": "run.opened", "turnId": "t1", "run": run})
}

/// Each event's run, in order.
pub fn runs(events: &[krowk_client::protocol::LogEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.body {
            LogBody::RunOpened { run, .. } => run.clone(),
            other => panic!("not a run.opened: {other:?}"),
        })
        .collect()
}
