//! A host's announcement, sealed to the person's devices (R-HOST-3,
//! R-E2E-1): what opens it, what refuses it, and what the sealed bytes do
//! not say. The device lists are built as `device_chain_corpus.rs` builds
//! its cases, with deterministic keys.

use krowk_client::device_chain::{Action, Chain, Entry, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, hex, DeviceId, DeviceKey, Label, SigningKey};
use krowk_client::presence::{self, AnnouncedInstance, Announcement};
use krowk_client::protocol::{LimitState, LimitStatus};
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeys};

const T: u64 = 1_790_000_000;

struct K {
    key: DeviceKey,
    signing: SigningKey,
    name: &'static str,
    kind: Kind,
}

impl K {
    fn device(n: u8, name: &'static str) -> K {
        K { key: DeviceKey::from_secret(&[n; 32]).unwrap(), signing: SigningKey::from_secret(&[n + 100; 32]).unwrap(), name, kind: Kind::Device }
    }

    fn kit(n: u8) -> K {
        let d = RecoveryKit::from_bytes([n; 16]).device();
        K { key: d.key, signing: d.signing, name: "recovery kit", kind: Kind::Recovery }
    }

    fn id(&self) -> DeviceId {
        self.key.id()
    }

    fn subject(&self) -> Subject {
        Subject { kind: self.kind, name: self.name.into(), os: if self.kind == Kind::Device { "linux".into() } else { String::new() }, device: self.key.public(), signing: self.signing.public() }
    }
}

fn user_key(g: u32) -> UserKey {
    UserKey::from_bytes(g, [g as u8; 32]).unwrap()
}

/// Generation 1 alone, or generation 2 holding a wrap of 1.
fn keys(g: u32) -> UserKeys {
    match g {
        1 => UserKeys::new(user_key(1), []).unwrap(),
        2 => UserKeys::new(user_key(2), [user_key(2).wrap_previous(&user_key(1)).unwrap()]).unwrap(),
        _ => unreachable!(),
    }
}

fn sign(e: &Entry, by: &[&K]) -> SignedEntry {
    e.sign(&by.iter().map(|k| (k.key.id(), &k.signing)).collect::<Vec<_>>()).unwrap()
}

fn after(prev: &SignedEntry, action: Action, subject: Subject, g: u32, time: u64) -> Entry {
    let seq = Entry::decode(&prev.bytes).unwrap().seq + 1;
    Entry { seq, prev: prev.hash(), action, subjects: vec![subject], generation: g, key_id: user_key(g).id(), time, signers: Vec::new() }
}

struct World {
    laptop: K,
    phone: K,
    tablet: K,
    kit: K,
}

fn world() -> World {
    World { laptop: K::device(1, "laptop"), phone: K::device(2, "phone"), tablet: K::device(3, "tablet"), kit: K::kit(1) }
}

/// The laptop and the kit, then the phone and the tablet, at generation 1.
fn entries(w: &World) -> Vec<SignedEntry> {
    let g = sign(&Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![w.laptop.subject(), w.kit.subject()], generation: 1, key_id: user_key(1).id(), time: T, signers: Vec::new() }, &[&w.laptop, &w.kit]);
    let p = sign(&after(&g, Action::Add, w.phone.subject(), 1, T + 1), &[&w.laptop]);
    let t = sign(&after(&p, Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop]);
    vec![g, p, t]
}

fn chain(w: &World) -> Chain {
    Chain::verify(&entries(w), None).unwrap()
}

/// The list after the phone removes `gone`, which rotates to generation 2.
fn chain_without(w: &World, gone: &K) -> Chain {
    let mut e = entries(w);
    let r = sign(&after(e.last().unwrap(), Action::Remove, gone.subject(), 2, T + 3), &[&w.phone]);
    e.push(r);
    Chain::verify(&e, None).unwrap()
}

fn record(me: &DeviceId) -> Announcement {
    Announcement {
        device: me.to_string(),
        name: "work laptop".into(),
        os: "linux".into(),
        version: "0.31.0".into(),
        online_since_ms: 1_790_000_000_000,
        instances: vec![AnnouncedInstance {
            name: "anthropic".into(),
            kind: "anthropic".into(),
            ready: true,
            state: "ready".into(),
            fix: None,
            models: vec!["claude-opus-5-5".into(), "claude-sonnet-5-5".into()],
            limit: Some(LimitStatus { status: LimitState::Warning, window: Some("five_hour".into()), used_percent: Some(81.0), resets_at_ms: Some(1_790_000_600_000) }),
        }],
        directories: vec!["/home/someone/Repositories/secret-project".into()],
        sessions: vec!["0123456789abcdef0123456789abcdef".into()],
    }
}

#[test]
fn r_host_3_an_announcement_sealed_on_one_device_opens_on_another() {
    let w = world();
    let (a, c) = (record(&w.laptop.id()), chain(&w));
    let sealed = presence::seal(&a, &keys(1), c.generation(), &w.laptop.id()).unwrap();
    // The phone holds the same user keys and the same verified list.
    let opened = presence::open(&sealed, &w.laptop.id(), &keys(1), &c).unwrap();
    assert_eq!(opened, Announcement { name: "laptop".into(), ..a });
}

#[test]
fn r_e2e_1_the_sealed_announcement_names_no_device_model_or_directory() {
    let w = world();
    let a = record(&w.laptop.id());
    let sealed = presence::seal(&a, &keys(1), 1, &w.laptop.id()).unwrap();
    let hexed = hex(&sealed);
    for secret in ["work laptop", "claude-opus-5-5", "/home/someone/Repositories/secret-project", "secret-project"] {
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} is in the sealed bytes");
        assert!(!hexed.contains(&hex(secret.as_bytes())), "{secret} is in the sealed bytes' hex");
    }
}

#[test]
fn r_e2e_1_an_announcement_opened_as_another_devices_is_refused() {
    let w = world();
    let c = chain(&w);
    let sealed = presence::seal(&record(&w.laptop.id()), &keys(1), 1, &w.laptop.id()).unwrap();
    // The registry moves the laptop's record to the tablet's row.
    let e = presence::open(&sealed, &w.tablet.id(), &keys(1), &c).unwrap_err();
    assert!(e.contains("does not open with your user key"), "{e}");
}

#[test]
fn r_e2e_1_an_announcement_under_a_generation_this_device_lacks_is_refused_by_generation() {
    let w = world();
    let c = chain_without(&w, &w.tablet);
    let sealed = presence::seal(&record(&w.laptop.id()), &keys(2), c.generation(), &w.laptop.id()).unwrap();
    let e = presence::open(&sealed, &w.laptop.id(), &keys(1), &c).unwrap_err();
    assert!(e.contains("generation 2"), "{e}");
}

#[test]
fn r_e2e_1_a_host_behind_the_lists_generation_does_not_seal() {
    let w = world();
    let e = presence::seal(&record(&w.laptop.id()), &keys(1), 2, &w.laptop.id()).unwrap_err();
    assert!(e.contains("generation 2"), "{e}");
}

#[test]
fn r_host_3_an_announcement_from_a_removed_device_is_refused() {
    let w = world();
    // The tablet seals before it learns it was removed.
    let sealed = presence::seal(&record(&w.tablet.id()), &keys(1), 1, &w.tablet.id()).unwrap();
    let e = presence::open(&sealed, &w.tablet.id(), &keys(2), &chain_without(&w, &w.tablet)).unwrap_err();
    assert!(e.contains("not on your device list"), "{e}");
}

#[test]
fn r_host_3_an_announcement_from_the_recovery_kit_is_refused() {
    let w = world();
    let sealed = presence::seal(&record(&w.kit.id()), &keys(1), 1, &w.kit.id()).unwrap();
    let e = presence::open(&sealed, &w.kit.id(), &keys(1), &chain(&w)).unwrap_err();
    assert!(e.contains("recovery kit"), "{e}");
}

#[test]
fn r_host_3_an_announcement_about_another_device_is_refused() {
    let w = world();
    // Sealed bound to the laptop, but saying it is the phone.
    let json = serde_json::to_vec(&record(&w.phone.id())).unwrap();
    let sealed = e2e::seal_to_person(&keys(1), 1, Label::Presence, &w.laptop.id().0, &json).unwrap();
    let e = presence::open(&sealed, &w.laptop.id(), &keys(1), &chain(&w)).unwrap_err();
    assert!(e.contains("about another device"), "{e}");
    // And `seal` will not write one.
    assert!(presence::seal(&record(&w.phone.id()), &keys(1), 1, &w.laptop.id()).is_err());
}

#[test]
fn r_host_3_an_announcement_over_its_bounds_is_refused_on_seal() {
    let w = world();
    let me = w.laptop.id();
    let long = Announcement { name: "x".repeat(65), ..record(&me) };
    assert!(presence::seal(&long, &keys(1), 1, &me).unwrap_err().contains("name"));
    let control = Announcement { directories: vec!["/tmp/\x1b[2Jgone".into()], ..record(&me) };
    assert!(presence::seal(&control, &keys(1), 1, &me).unwrap_err().contains("control character"));
    let mut many = record(&me);
    many.instances = vec![many.instances[0].clone(); 33];
    assert!(presence::seal(&many, &keys(1), 1, &me).unwrap_err().contains("instances"));
    let mut models = record(&me);
    models.instances[0].models = (0..21).map(|n| format!("model-{n}")).collect();
    assert!(presence::seal(&models, &keys(1), 1, &me).unwrap_err().contains("models"));
    let big = Announcement { sessions: vec!["0123456789abcdef0123456789abcdef".into(); 256], directories: vec!["/d".repeat(100); 64], ..record(&me) };
    assert!(presence::seal(&big, &keys(1), 1, &me).unwrap_err().contains("bytes"));
}

#[test]
fn r_host_3_an_announcement_over_its_bounds_is_refused_on_open() {
    let w = world();
    let me = w.laptop.id();
    let long = Announcement { os: "y".repeat(33), ..record(&me) };
    let sealed = e2e::seal_to_person(&keys(1), 1, Label::Presence, &me.0, &serde_json::to_vec(&long).unwrap()).unwrap();
    assert!(presence::open(&sealed, &me, &keys(1), &chain(&w)).unwrap_err().contains("OS"));
}

#[test]
fn r_e2e_1_records_listing_one_and_three_directories_seal_to_the_same_length() {
    let w = world();
    let me = w.laptop.id();
    let one = record(&me);
    let three = Announcement { directories: vec!["/home/someone/a".into(), "/home/someone/b".into(), "/home/someone/c".into()], ..record(&me) };
    let (a, b) = (presence::seal(&one, &keys(1), 1, &me).unwrap(), presence::seal(&three, &keys(1), 1, &me).unwrap());
    assert_eq!(a.len(), b.len());
    // The padded JSON is a whole number of 1,024-byte blocks.
    let plain = e2e::open_from_person(&a, Label::Presence, &me.0, &keys(1)).unwrap();
    assert_eq!(plain.len() % presence::PAD, 0);
}

#[test]
fn r_host_3_the_opened_name_and_os_are_the_device_lists() {
    let w = world();
    let me = w.laptop.id();
    let claim = Announcement { name: "the phone".into(), os: "ios".into(), ..record(&me) };
    let opened = presence::open(&presence::seal(&claim, &keys(1), 1, &me).unwrap(), &me, &keys(1), &chain(&w)).unwrap();
    assert_eq!((opened.name.as_str(), opened.os.as_str()), ("laptop", "linux"));
}

#[test]
fn r_host_3_a_newer_hosts_fields_are_ignored() {
    let w = world();
    let me = w.laptop.id();
    let json = format!(r#"{{"device":"{me}","name":"laptop","os":"linux","version":"9.0.0","onlineSinceMs":1,"gpu":"lots","instances":[{{"name":"a","kind":"a","ready":false,"state":"keyNotSet","weight":3}}]}}"#);
    let sealed = e2e::seal_to_person(&keys(1), 1, Label::Presence, &me.0, json.as_bytes()).unwrap();
    let opened = presence::open(&sealed, &me, &keys(1), &chain(&w)).unwrap();
    assert_eq!((opened.version.as_str(), opened.instances[0].state.as_str(), opened.directories.len()), ("9.0.0", "keyNotSet", 0));
}

#[test]
fn r_e2e_1_a_changed_byte_does_not_open() {
    let w = world();
    let me = w.laptop.id();
    let sealed = presence::seal(&record(&me), &keys(1), 1, &me).unwrap();
    for i in [1, 3, 10, 60, sealed.len() - 1] {
        let mut b = sealed.clone();
        b[i] ^= 1;
        assert!(presence::open(&b, &me, &keys(1), &chain(&w)).is_err(), "byte {i}");
    }
    let mut newer = sealed.clone();
    newer[0] = 2;
    assert!(presence::open(&newer, &me, &keys(1), &chain(&w)).unwrap_err().contains("upgrade krowk"));
}
