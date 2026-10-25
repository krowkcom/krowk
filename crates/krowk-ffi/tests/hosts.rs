//! R-HOST-3, R-HOST-4, R-HAND-4: the paired phone lists the person's hosts
//! from their sealed announcements (`Account::list_hosts`) and starts a
//! session on one (`Account::start_session`), against the stand-in
//! registry, with the host played by the test as krowk-harness's presence
//! thread plays it: publishing, taking the delivered command, answering.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Bridge, World, key, subject};
use krowk_api::Client;
use krowk_api::sync::ListPost;
use krowk_client::device_chain::{Batch, Chain, Change, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceId, DeviceKey, DeviceSigner, SigningKey};
use krowk_client::host_command::{self, HostAnswer, HostCommand};
use krowk_client::keystore::Keystore;
use krowk_client::presence::{self, AnnouncedInstance, Announcement};
use krowk_client::protocol::{LimitState, LimitStatus, ModelRef};
use krowk_client::user_key::{UserKey, UserKeys};
use krowk_ffi::{KrowkError, StartAnswer, new_command_id};

/// The list as the registry serves it, verified from entry 0.
fn served(w: &World) -> Chain {
    let list = w.host.device_list_all().unwrap();
    let entries: Vec<SignedEntry> = list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect();
    Chain::verify(&entries, None).unwrap()
}

fn post_of(batch: &Batch) -> ListPost {
    ListPost {
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over: false,
    }
}

/// The host signs `changes` onto the list, rotating the key for a removal,
/// as `krowk devices` does; the user key it leaves current.
fn host_posts(w: &World, held: &UserKey, changes: Vec<Change<'static>>, time: u64) -> UserKey {
    let (_, batch) = served(w).batch(held, changes, w.host_device.id(), &w.host_signing, time).unwrap();
    w.host.append_device_list(&post_of(&batch)).unwrap();
    batch.newest
}

/// What a host announces: one ready instance with a limit, one not ready
/// with its fix, and one directory.
fn announcement(device: DeviceId, version: &str) -> Announcement {
    Announcement {
        device: device.to_string(),
        name: "what the host claims".into(),
        os: "linux".into(),
        version: version.into(),
        online_since_ms: 1_790_000_000_000,
        instances: vec![
            AnnouncedInstance {
                name: "anthropic".into(),
                kind: "anthropic".into(),
                ready: true,
                state: "ready".into(),
                fix: None,
                models: vec!["claude-sonnet-4-6".into(), "claude-opus-4-1".into()],
                limit: Some(LimitStatus { status: LimitState::Warning, window: Some("five_hour".into()), used_percent: Some(82.0), resets_at_ms: None }),
            },
            AnnouncedInstance { name: "openai".into(), kind: "openai".into(), ready: false, state: "keyNotSet".into(), fix: Some("set OPENAI_API_KEY".into()), models: Vec::new(), limit: None },
        ],
        directories: vec!["/home/ann/code/app".into()],
        sessions: Vec::new(),
    }
}

/// `device` publishes its announcement through `api`, sealed under `user`.
fn publish(api: &Client, device: DeviceId, version: &str, user: &UserKey) {
    let keys = UserKeys::new(user.clone(), []).unwrap();
    let sealed = presence::seal(&announcement(device, version), &keys, user.generation(), &device).unwrap();
    api.put_presence(&sealed, 30).unwrap();
}

/// Another device's own signed client, its key bound to it.
fn device_client(w: &World, name: &str, d: &DeviceKey, s: &SigningKey) -> Client {
    let c = Client::new(&w.api, &key(name)).signed_by(DeviceSigner::new(d.id(), SigningKey::from_secret(&*s.secret_bytes()).unwrap()).shared());
    c.claim_key_device().unwrap();
    c
}

#[test]
fn r_host_3_a_published_presence_lists_online_with_its_directories_and_ready_models() {
    let w = World::new();
    publish(&w.host, w.host_device.id(), "0.31.0", &w.user);
    let list = w.phone.list_hosts().unwrap();
    assert_eq!(list.unreadable, 0);
    assert_eq!(list.hosts.len(), 1, "{list:?}");
    let h = &list.hosts[0];
    assert_eq!(h.device, w.host_device.id().to_string());
    assert_eq!(h.name, "host", "the device list's name, not the host's claim");
    assert!(h.online);
    assert!(h.last_seen_ms > 1_700_000_000_000, "{}", h.last_seen_ms);
    assert_eq!(h.version, "0.31.0");
    assert_eq!(h.directories, ["/home/ann/code/app"]);
    let ready = &h.instances[0];
    assert!(ready.ready);
    assert_eq!(ready.models, ["claude-sonnet-4-6", "claude-opus-4-1"]);
    assert_eq!(ready.limit.as_ref().map(|l| (l.status.clone(), l.used_percent)), Some((LimitState::Warning, Some(82.0))));
    let unready = &h.instances[1];
    assert!(!unready.ready);
    assert_eq!(unready.fix.as_deref(), Some("set OPENAI_API_KEY"));
}

#[test]
fn r_hand_4_this_phone_is_never_listed_as_a_host() {
    let w = World::new();
    // The phone's own key, signed by its own device keys: were it ever to
    // publish, it still isn't a host to itself.
    let ks = Keystore::with(Arc::new(Bridge(w.storage.clone())));
    let (d, s) = (ks.device_key().unwrap(), ks.signing_key().unwrap());
    let phone = Client::new(&w.api, &key("phone")).signed_by(DeviceSigner::new(d.id(), s).shared());
    publish(&phone, d.id(), "dev", &w.user);
    publish(&w.host, w.host_device.id(), "0.31.0", &w.user);
    let list = w.phone.list_hosts().unwrap();
    assert_eq!(list.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["host"]);
    assert_eq!(list.unreadable, 0);
}

#[test]
fn r_host_3_online_hosts_come_first_then_by_name() {
    let w = World::new();
    let mut made = Vec::new();
    for name in ["zeta", "alpha"] {
        let (d, s) = (DeviceKey::generate(), SigningKey::generate());
        host_posts(&w, &w.user, vec![Change::Add(subject(name, &d, &s))], common::T0 + 10 + made.len() as u64);
        made.push((name, d, s));
    }
    w.phone.check_device_list().unwrap();
    for (name, d, s) in &made {
        publish(&device_client(&w, name, d, s), d.id(), "0.31.0", &w.user);
    }
    publish(&w.host, w.host_device.id(), "0.31.0", &w.user);
    let names: Vec<_> = w.phone.list_hosts().unwrap().hosts.into_iter().map(|h| h.name).collect();
    assert_eq!(names, ["alpha", "host", "zeta"]);
}

#[test]
fn r_host_3_a_removed_devices_presence_is_not_listed() {
    let w = World::new();
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    let held = host_posts(&w, &w.user, vec![Change::Add(subject("old laptop", &d, &s))], common::T0 + 10);
    w.phone.check_device_list().unwrap();
    publish(&device_client(&w, "old laptop", &d, &s), d.id(), "0.31.0", &held);
    assert!(w.phone.list_hosts().unwrap().hosts.iter().any(|h| h.name == "old laptop"), "listed while on the list");

    let chain = served(&w);
    let listed = chain.devices().iter().find(|x| x.name == "old laptop").unwrap();
    let newest = host_posts(&w, &held, vec![Change::Remove(Subject::of(listed))], common::T0 + 20);
    w.phone.check_device_list().unwrap();
    publish(&w.host, w.host_device.id(), "0.31.0", &newest);
    let list = w.phone.list_hosts().unwrap();
    assert_eq!(list.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["host"], "{list:?}");
}

#[test]
fn r_host_3_an_announcement_the_phone_cannot_open_is_counted() {
    let w = World::new();
    // A device the phone's pinned list doesn't name yet.
    let (d, s) = (DeviceKey::generate(), SigningKey::generate());
    host_posts(&w, &w.user, vec![Change::Add(subject("new desktop", &d, &s))], common::T0 + 10);
    publish(&device_client(&w, "new desktop", &d, &s), d.id(), "0.31.0", &w.user);
    let list = w.phone.list_hosts().unwrap();
    assert!(list.hosts.is_empty(), "{list:?}");
    assert_eq!(list.unreadable, 1);
}

/// The host as its presence thread plays it, on a thread of its own: it
/// refreshes its announcement until a command comes, opens it, checks it
/// with `check`, answers `answer` and returns what it opened.
fn host_answers(w: &World, answer: HostAnswer) -> std::thread::JoinHandle<HostCommand> {
    let host = Client::new(&w.api, &key("host")).signed_by(DeviceSigner::new(w.host_device.id(), SigningKey::from_secret(&*w.host_signing.secret_bytes()).unwrap()).shared());
    let (me, chain, user) = (w.host_device.id(), served(w), w.user.clone());
    let keys = UserKeys::new(user.clone(), []).unwrap();
    std::thread::spawn(move || {
        let sealed = presence::seal(&announcement(me, "0.31.0"), &keys, user.generation(), &me).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let put = host.put_presence(&sealed, 30).unwrap();
            if let Some(c) = put.commands.first() {
                let from = DeviceId::parse(&c.from).unwrap();
                let command = host_command::open_command(&c.sealed, &c.id, &from, &me, &keys, &chain).unwrap().unwrap();
                let reply = host_command::seal_answer(&answer, &c.id, &me, &from, &keys, user.generation()).unwrap();
                host.answer_host_command(&c.id, &reply).unwrap();
                return command;
            }
            assert!(Instant::now() < deadline, "no command came");
            std::thread::sleep(Duration::from_millis(100));
        }
    })
}

#[test]
fn r_host_4_a_start_the_host_answers_returns_started_and_names_no_permission_mode() {
    let w = World::new();
    publish(&w.host, w.host_device.id(), "0.31.0", &w.user);
    let session = "0199c0de-0000-7000-8000-000000000001";
    let host = host_answers(&w, HostAnswer::Started { session_id: session.into() });
    let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
    let id = new_command_id();
    let answer = w.phone.start_session(w.host_device.id().to_string(), "/home/ann/code/app".into(), model.clone(), "fix the tests".into(), id).unwrap();
    assert_eq!(answer, StartAnswer::Started { session_id: session.into() });
    // What reached the host: the place, the model and the words, and no
    // more — the command has no field for a mode, a toolset, an effort or
    // a budget.
    let HostCommand::Start { cwd, model: got, prompt } = host.join().unwrap();
    assert_eq!((cwd.as_str(), got, prompt.as_str()), ("/home/ann/code/app", model, "fix the tests"));
}

#[test]
fn r_host_4_a_refusal_is_the_hosts_own_words() {
    let w = World::new();
    let host = host_answers(&w, HostAnswer::refused("model_not_ready", "openai isn't ready on host — set OPENAI_API_KEY"));
    let model = ModelRef { instance: "openai".into(), model: "gpt-5".into() };
    let answer = w.phone.start_session(w.host_device.id().to_string(), "/home/ann/code/app".into(), model, "hi".into(), new_command_id()).unwrap();
    assert_eq!(answer, StartAnswer::Refused { code: "model_not_ready".into(), message: "openai isn't ready on host — set OPENAI_API_KEY".into() });
    host.join().unwrap();
}

#[test]
fn r_host_4_the_same_id_again_is_the_same_command() {
    let w = World::new();
    let session = "0199c0de-0000-7000-8000-000000000002";
    let host = host_answers(&w, HostAnswer::Started { session_id: session.into() });
    let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
    let id = new_command_id();
    let start = || w.phone.start_session(w.host_device.id().to_string(), "/home/ann/code/app".into(), model.clone(), "hi".into(), id.clone());
    assert_eq!(start().unwrap(), StartAnswer::Started { session_id: session.into() });
    host.join().unwrap();
    // A retry of the same start, with no host now: the stored answer.
    assert_eq!(start().unwrap(), StartAnswer::Started { session_id: session.into() });
}

#[test]
fn r_host_4_what_is_no_start_is_refused_before_anything_is_sent() {
    let w = World::new();
    let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
    let host = w.host_device.id().to_string();
    let invalid = |r: Result<StartAnswer, KrowkError>| matches!(r, Err(KrowkError::InvalidInput { .. }));
    assert!(invalid(w.phone.start_session("not a device".into(), "/a".into(), model.clone(), "hi".into(), new_command_id())));
    assert!(invalid(w.phone.start_session(host.clone(), "/a".into(), model.clone(), "hi".into(), "not-a-uuid".into())));
    assert!(invalid(w.phone.start_session(host, "/a".into(), model.clone(), "x".repeat(300 * 1024), new_command_id())));
    let stranger = DeviceKey::generate().id().to_string();
    match w.phone.start_session(stranger, "/a".into(), model, "hi".into(), new_command_id()) {
        Err(KrowkError::Refused { code, .. }) => assert_eq!(code, "no_host"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn r_host_4_command_ids_are_fresh_version_4_uuids() {
    let (a, b) = (new_command_id(), new_command_id());
    assert_ne!(a, b);
    for id in [a, b] {
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4", "{id}");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
        assert_eq!(id, id.to_lowercase());
    }
}
