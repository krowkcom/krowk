//! R-IOS-3, R-CLIENT-1, R-PERF-6: a paired phone lists the person's synced
//! sessions from their sealed indexes and opens one from its latest
//! checkpoint, all in krowk-client behind `Account`, never in Swift.
//! Against the stand-in registry, with a host that writes the session at
//! rest through the harness's own writer, and the phone on the device list
//! the host started.

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use krowk_api::Client;
use krowk_client::Zeroizing;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::e2e::{self, DeviceKey, DeviceSigner, SessionKey, SigningKey};
use krowk_client::keystore::{self, Keystore, Slot, StorageLock};
use krowk_client::protocol::LogBody;
use krowk_client::session_record;
use krowk_client::user_key::UserKeys;
use krowk_ffi::{Account, HeadRecord, KeyStorage, KrowkError};
use krowk_harness::sync::store::{Index, Writer};
use serde_json::{Value, json};

const TOKEN: &str = "krowk_sk_ffi_sessions_00000000000000000";
/// When the list is made, by its devices' clocks.
const T0: u64 = 1_790_000_000;

/// The phone's Keychain, in memory.
#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

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
struct Bridge(Arc<Memory>);

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
fn key(name: &str) -> String {
    format!("{TOKEN}#{name}")
}

/// A phone's storage holding the key `token`, as sign-in leaves it.
fn signed_in(token: &str) -> Arc<Memory> {
    let m = Memory::default();
    let creds = json!({"version": 1, "token": token, "key_id": "key_phone", "workspace": "ws", "workspace_name": "Workspace"});
    m.write("credentials.json".into(), serde_json::to_vec(&creds).unwrap()).unwrap();
    Arc::new(m)
}

fn subject(name: &str, device: &DeviceKey, signing: &SigningKey) -> Subject {
    Subject { kind: Kind::Device, name: name.into(), os: "test".into(), device: device.public(), signing: signing.public() }
}

fn post(entries: &[krowk_client::device_chain::SignedEntry], wraps: Vec<(String, String)>) -> krowk_api::sync::ListPost {
    krowk_api::sync::ListPost { entries: entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(), links: Vec::new(), wraps, start_over: false }
}

/// A host on a new device list, the phone added to it, both bound to keys
/// of their own in the registry, and the phone's storage holding what
/// pairing leaves: its device keys, the list and the user keys.
struct World {
    _registry: krowk_devregistry::Running,
    api: String,
    host: Arc<Client>,
    host_device: DeviceKey,
    host_signing: SigningKey,
    user: krowk_client::user_key::UserKey,
    phone: Arc<Account>,
}

impl World {
    fn new() -> World {
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

        let phone = Account::open(api.clone(), storage).unwrap();
        assert_eq!(phone.device_id(), phone_device.id().to_string());
        World { _registry: registry, api, host: Arc::new(host), host_device, host_signing, user, phone }
    }

    /// A session record for `id` under a new session key, signed by
    /// `signer` (its id, its key), with the index titled `title`; the key.
    fn record(&self, id: &str, title: &str, signer: (e2e::DeviceId, &SigningKey)) -> (SessionKey, String, Index) {
        let raw = krowk_client::sync::uuid(id);
        let key = SessionKey::generate();
        let sealed_key = e2e::wrap_session_key(&key, &raw, &self.user);
        let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, &self.user, signer.1).unwrap();
        let wrapped = e2e::hex(&sealed_key);
        let index = Index { title: title.into(), cwd: "/src/app".into(), ..Default::default() };
        let sealed_index = e2e::hex(&e2e::seal_session_index(&key, &raw, &serde_json::to_vec(&index).unwrap()));
        self.host.put_sync_session(id, &wrapped, Some((&e2e::hex(&signature), &signer.0.to_string())), Some(&sealed_index), None).unwrap();
        (key, wrapped, index)
    }

    /// The host takes session `id` up and writes `first` then a checkpoint
    /// then `then`: the writer, holding the lease, and its token.
    fn host_session(&self, id: &str, title: &str, first: &[Value], then: &[Value]) -> (Writer, String) {
        let (key, wrapped, index) = self.record(id, title, (self.host_device.id(), &self.host_signing));
        let lease = self.host.acquire_lease(id, &self.host_device.id().to_string(), 60, "development").unwrap();
        let mut w = Writer::take_up(self.host.clone(), key.into(), id, wrapped, index, lease.fence).unwrap();
        for e in first {
            w.push(e.clone());
        }
        w.checkpoint(None, &lease.token).unwrap();
        for e in then {
            w.push(e.clone());
        }
        w.flush(&lease.token).unwrap();
        (w, lease.token)
    }
}

/// A log event the phone decodes: a run opened, named `run`.
fn event(session: &str, n: u32, run: &str) -> Value {
    json!({"id": format!("01a0ec7b-2222-7000-8000-{n:012}"), "sessionId": session, "timeMs": n, "type": "run.opened", "turnId": "t1", "run": run})
}

/// Each event's run, in order.
fn runs(events: &[krowk_client::protocol::LogEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.body {
            LogBody::RunOpened { run, .. } => run.clone(),
            other => panic!("not a run.opened: {other:?}"),
        })
        .collect()
}

fn refused<T>(r: Result<T, KrowkError>) -> String {
    match r {
        Err(KrowkError::Refused { code, .. }) => code,
        Err(e) => panic!("not a refusal: {e:?}"),
        Ok(_) => panic!("not refused"),
    }
}

const SESSION: &str = "01a0ec7b-3333-7000-8000-000000000079";

#[test]
fn r_ios_3_list_sessions_shows_the_index_and_counts_a_record_signed_off_the_list_unreadable() {
    let w = World::new();
    let _host = w.host_session(SESSION, "fix the build", &[event(SESSION, 1, "first")], &[]);

    let list = w.phone.list_sessions().unwrap();
    assert_eq!(list.unreadable, 0, "{list:?}");
    assert_eq!(list.sessions.len(), 1, "{list:?}");
    let s = &list.sessions[0];
    assert_eq!((s.id.as_str(), s.title.as_str(), s.cwd.as_str()), (SESSION, "fix the build", "/src/app"));
    assert!(s.updated_ms > 0, "the index's write time: {s:?}");
    assert_eq!(s.hosted_by.as_deref(), Some(w.host_device.id().to_string().as_str()), "the lease holder: {s:?}");
    assert!(s.head.as_ref().is_some_and(|h| h.digest.len() == 32), "{s:?}");

    // A record a device the list never held signed: left out, counted.
    let (stranger, stranger_signing) = (DeviceKey::generate(), SigningKey::generate());
    w.record("01a0ec7b-3333-7000-8000-0000000000ff", "forged", (stranger.id(), &stranger_signing));
    let list = w.phone.list_sessions().unwrap();
    assert_eq!(list.unreadable, 1, "{list:?}");
    assert_eq!(list.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [SESSION]);
}

#[test]
fn r_ios_3_r_perf_6_open_session_reads_from_the_checkpoint_and_catch_up_returns_only_whats_new() {
    let w = World::new();
    let (mut host, token) = w.host_session(SESSION, "fix the build", &[event(SESSION, 1, "before")], &[event(SESSION, 2, "after")]);

    let opened = w.phone.open_session(SESSION.into(), None, None).unwrap();
    assert_eq!(opened.id(), SESSION);
    assert_eq!(opened.title(), "fix the build");
    // The checkpoint's compacted context, then the tail after it.
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after"]);
    let head = opened.head().expect("a head");
    // Chunks 0 (events), 1 (checkpoint), 2 (the tail).
    assert_eq!(head.index, 2);
    assert!(opened.catch_up().unwrap().is_empty(), "nothing written since");

    host.push(event(SESSION, 3, "later"));
    host.push(event(SESSION, 4, "latest"));
    host.flush(&token).unwrap();
    assert_eq!(runs(&opened.catch_up().unwrap()), ["later", "latest"]);
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after", "later", "latest"]);
    assert_eq!(opened.head().unwrap().index, 3);
    assert!(opened.catch_up().unwrap().is_empty());

    // Opened again, knowing the head it saw: the same log reads.
    let again = w.phone.open_session(SESSION.into(), opened.head(), None).unwrap();
    assert_eq!(again.head(), opened.head());
}

#[test]
fn r_ios_3_open_session_beyond_what_is_served_is_session_rolled_back() {
    let w = World::new();
    let _host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let served = w.phone.open_session(SESSION.into(), None, None).unwrap().head().unwrap();
    let beyond = HeadRecord { index: served.index + 3, digest: vec![7; 32] };
    assert_eq!(refused(w.phone.open_session(SESSION.into(), Some(beyond), None)), "session_rolled_back");
    // Another chunk at the head it knows: another log.
    let forked = HeadRecord { index: served.index, digest: vec![7; 32] };
    assert_eq!(refused(w.phone.open_session(SESSION.into(), Some(forked), None)), "session_rolled_back");
    let short = HeadRecord { index: 0, digest: vec![7; 31] };
    assert!(matches!(w.phone.open_session(SESSION.into(), Some(short), None), Err(KrowkError::InvalidInput { .. })));
}

#[test]
fn r_client_1_relay_ticket_and_the_relay_the_phone_uses() {
    let w = World::new();
    let _host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let opened = w.phone.open_session(SESSION.into(), None, Some("ws://127.0.0.1:9".into())).unwrap();
    assert!(!opened.relay_ticket().unwrap().is_empty());

    assert_ne!(w.phone.relay_url(None), "wss://relay.krowk.com", "the stand-in's API is not krowk's");
    assert_eq!(w.phone.relay_url(Some(" ws://127.0.0.1:9 ".into())), "ws://127.0.0.1:9");
    assert_eq!(w.phone.relay_env(), "development");
    let hosted = Account::open(krowk_api::DEFAULT_BASE_URL.into(), signed_in(TOKEN)).unwrap();
    assert_eq!(hosted.relay_url(None), "wss://relay.krowk.com");
    assert_eq!(hosted.relay_url(Some(String::new())), "wss://relay.krowk.com");
    assert_eq!(hosted.relay_env(), if cfg!(debug_assertions) { "development" } else { "production" });
}

#[test]
fn r_client_1_an_account_with_no_user_keys_is_not_paired() {
    let w = World::new();
    let unpaired = Account::open(w.api.clone(), signed_in(&key("phone"))).unwrap();
    assert_eq!(refused(unpaired.list_sessions()), "not_paired");
    assert_eq!(refused(unpaired.open_session(SESSION.into(), None, None)), "not_paired");
}
