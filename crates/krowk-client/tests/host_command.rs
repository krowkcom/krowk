//! A command one device sends a host, and its answer, sealed to the
//! person's devices and bound to the command's id and both devices
//! (R-HOST-4, R-E2E-1): what opens, what refuses, and what is dropped
//! unread. The device lists are built as `presence.rs` builds them.

use krowk_client::device_chain::{Action, Chain, Entry, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, hex, DeviceId, DeviceKey, Label, SigningKey};
use krowk_client::host_command::{self, HostAnswer, HostCommand};
use krowk_client::protocol::ModelRef;
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeys};

const T: u64 = 1_790_000_000;
const ID: &str = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e5f";
const OTHER_ID: &str = "0190f3c1-8a2b-7c4d-9e5f-0a1b2c3d4e60";

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

/// The host, the phone that sends it commands, a tablet and the kit.
struct World {
    host: K,
    phone: K,
    tablet: K,
    kit: K,
}

fn world() -> World {
    World { host: K::device(1, "host"), phone: K::device(2, "phone"), tablet: K::device(3, "tablet"), kit: K::kit(1) }
}

fn entries(w: &World) -> Vec<SignedEntry> {
    let g = sign(&Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![w.host.subject(), w.kit.subject()], generation: 1, key_id: user_key(1).id(), time: T, signers: Vec::new() }, &[&w.host, &w.kit]);
    let p = sign(&after(&g, Action::Add, w.phone.subject(), 1, T + 1), &[&w.host]);
    let t = sign(&after(&p, Action::Add, w.tablet.subject(), 1, T + 2), &[&w.host]);
    vec![g, p, t]
}

fn chain(w: &World) -> Chain {
    Chain::verify(&entries(w), None).unwrap()
}

/// The list after the host removes `gone`, which rotates to generation 2.
fn chain_without(w: &World, gone: &K) -> Chain {
    let mut e = entries(w);
    let r = sign(&after(e.last().unwrap(), Action::Remove, gone.subject(), 2, T + 3), &[&w.host]);
    e.push(r);
    Chain::verify(&e, None).unwrap()
}

fn start() -> HostCommand {
    HostCommand::Start { cwd: "/home/someone/secret-project".into(), model: ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() }, prompt: "fix the secret-flaky test".into() }
}

#[test]
fn r_host_4_a_command_and_its_answer_open_on_the_other_side() {
    let w = world();
    let c = chain(&w);
    let sealed = host_command::seal_command(&start(), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(start()));
    // The registry keeps ids lower case: an id sent upper case still opens.
    let upper = host_command::seal_command(&start(), &ID.to_uppercase(), &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert!(host_command::open_command(&upper, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap().is_ok());

    for answer in [HostAnswer::Started { session_id: "0190f3c2-0000-7000-8000-000000000001".into() }, HostAnswer::refused("no_instance", "no instance nope — add one")] {
        let sealed = host_command::seal_answer(&answer, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
        assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), answer);
    }
}

#[test]
fn r_e2e_1_a_sealed_command_names_no_directory_model_or_prompt() {
    let w = world();
    let sealed = host_command::seal_command(&start(), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    let hexed = hex(&sealed);
    for secret in ["secret-project", "claude-opus-5-5", "secret-flaky"] {
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} is in the sealed bytes");
        assert!(!hexed.contains(&hex(secret.as_bytes())), "{secret} is in the sealed bytes' hex");
    }
}

#[test]
fn r_e2e_1_a_command_moved_to_another_id_or_pair_of_devices_does_not_open() {
    let w = world();
    let c = chain(&w);
    let sealed = host_command::seal_command(&start(), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    let moved = [
        (OTHER_ID, w.phone.id(), w.host.id()),
        (ID, w.tablet.id(), w.host.id()),
        (ID, w.phone.id(), w.tablet.id()),
        (ID, w.host.id(), w.phone.id()),
    ];
    for (id, from, to) in moved {
        let e = host_command::open_command(&sealed, id, &from, &to, &keys(1), &c).unwrap_err();
        assert!(e.contains("does not open"), "{id} {from} {to}: {e}");
    }
    // An answer is no command, and a command no answer.
    let answer = host_command::seal_answer(&HostAnswer::refused("x", "y"), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert!(host_command::open_command(&answer, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).is_err());
    assert!(host_command::open_answer(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).is_err());
    // Nor does an answer moved to another command.
    let answer = host_command::seal_answer(&HostAnswer::refused("x", "y"), ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
    assert!(host_command::open_answer(&answer, OTHER_ID, &w.host.id(), &w.phone.id(), &keys(1), &c).is_err());
}

#[test]
fn r_host_4_a_command_from_a_removed_device_is_refused() {
    let w = world();
    // The tablet seals before it learns it was removed.
    let sealed = host_command::seal_command(&start(), ID, &w.tablet.id(), &w.host.id(), &keys(1), 1).unwrap();
    let e = host_command::open_command(&sealed, ID, &w.tablet.id(), &w.host.id(), &keys(2), &chain_without(&w, &w.tablet)).unwrap_err();
    assert!(e.contains("not on your device list"), "{e}");
    // And one the removed tablet writes as the phone's, under the
    // generation it still holds, is refused by generation.
    let forged = host_command::seal_command(&start(), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    let e = host_command::open_command(&forged, ID, &w.phone.id(), &w.host.id(), &keys(2), &chain_without(&w, &w.tablet)).unwrap_err();
    assert!(e.contains("generation 1, older than the generation 2"), "{e}");
}

#[test]
fn r_host_4_a_command_from_the_recovery_kit_is_refused() {
    let w = world();
    let sealed = host_command::seal_command(&start(), ID, &w.kit.id(), &w.host.id(), &keys(1), 1).unwrap();
    let e = host_command::open_command(&sealed, ID, &w.kit.id(), &w.host.id(), &keys(1), &chain(&w)).unwrap_err();
    assert!(e.contains("recovery kit"), "{e}");
}

/// The remote side picks the place, the model and the words, never the
/// permissions: what else a command carries opens, and is dropped.
#[test]
fn r_host_4_a_commands_extra_fields_are_dropped_unread() {
    let w = world();
    let json = br#"{"type":"start","cwd":"/w","model":{"instance":"anthropic","model":"m"},"prompt":"hi","permissionMode":"unhinged","toolset":"grok","effort":"max","budget":{"maxUsd":1000}}"#;
    let sealed = e2e::seal_to_person(&keys(1), 1, Label::HostCommand, &host_command::binding(ID, &w.phone.id(), &w.host.id()), json).unwrap();
    let opened = host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &chain(&w)).unwrap().unwrap();
    assert_eq!(opened, HostCommand::Start { cwd: "/w".into(), model: ModelRef { instance: "anthropic".into(), model: "m".into() }, prompt: "hi".into() });
}

#[test]
fn r_host_4_an_unknown_command_is_answered_unknown_command() {
    let w = world();
    let seal = |json: &[u8]| e2e::seal_to_person(&keys(1), 1, Label::HostCommand, &host_command::binding(ID, &w.phone.id(), &w.host.id()), json).unwrap();
    let open = |sealed: &[u8]| host_command::open_command(sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &chain(&w)).unwrap();
    let Err(HostAnswer::Refused { code, .. }) = open(&seal(br#"{"type":"takeUp","session":"s"}"#)) else { panic!("opened") };
    assert_eq!(code, "unknown_command");
    let Err(HostAnswer::Refused { code, .. }) = open(&seal(br#"{"type":"start","cwd":"/w"}"#)) else { panic!("opened") };
    assert_eq!(code, "bad_command");
    let long = serde_json::json!({"type": "start", "cwd": "/w", "model": {"instance": "a", "model": "m"}, "prompt": "x".repeat(host_command::MAX_PROMPT + 1)});
    let Err(HostAnswer::Refused { code, .. }) = open(&seal(&serde_json::to_vec(&long).unwrap())) else { panic!("opened") };
    assert_eq!(code, "prompt_too_long", "the host holds the limit too");
}

#[test]
fn r_host_4_a_command_past_what_the_registry_carries_is_refused_on_seal() {
    let w = world();
    let HostCommand::Start { cwd, model, .. } = start() else { unreachable!() };
    let long = HostCommand::Start { cwd, model, prompt: "x".repeat(host_command::MAX_SEALED) };
    let e = host_command::seal_command(&long, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap_err();
    assert!(e.contains("shorten the prompt"), "{e}");
}

#[test]
fn r_host_4_a_long_refusal_is_cut_to_fit_what_the_registry_carries() {
    let w = world();
    // Plain, wide, and what JSON escapes six times over: each fits.
    for message in ["é".repeat(host_command::MAX_ANSWER), "\"".repeat(host_command::MAX_ANSWER), "\u{1b}".repeat(host_command::MAX_ANSWER)] {
        let long = HostAnswer::refused(&"\u{7}".repeat(200), message);
        let sealed = host_command::seal_answer(&long, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
        assert!(sealed.len() <= host_command::MAX_ANSWER);
        let HostAnswer::Refused { code, message } = host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &chain(&w)).unwrap() else { panic!() };
        assert!(serde_json::to_string(&code).unwrap().len() <= host_command::MAX_CODE + 2 && serde_json::to_string(&message).unwrap().len() <= host_command::MAX_MESSAGE + 2 && message.ends_with('…'), "{} {}", code.len(), message.len());
    }
}

/// R-HAND-4, R-HOST-4: a `take`, the phone asking a host to take a session
/// up, opens on the host as sent, with unknown fields dropped; its `Taken`
/// answer opens on the phone; and a session id that isn't one is refused
/// before the host passes it anywhere.
#[test]
fn r_hand_4_a_take_and_its_answer_open_on_the_other_side() {
    let w = world();
    let c = chain(&w);
    let take = HostCommand::Take { session_id: "0190f3c2-0000-7000-8000-000000000001".into(), cwd: "/home/someone/clone".into() };
    let sealed = host_command::seal_command(&take, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(take.clone()));
    let taken = HostAnswer::Taken { session_id: "0190f3c2-0000-7000-8000-000000000001".into() };
    let sealed = host_command::seal_answer(&taken, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), taken);

    let seal = |json: &[u8]| e2e::seal_to_person(&keys(1), 1, Label::HostCommand, &host_command::binding(ID, &w.phone.id(), &w.host.id()), json).unwrap();
    let open = |sealed: &[u8]| host_command::open_command(sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap();
    let extra = br#"{"type":"take","sessionId":"0190f3c2-0000-7000-8000-000000000001","cwd":"/w","permissionMode":"unhinged"}"#;
    assert_eq!(open(&seal(extra)), Ok(HostCommand::Take { session_id: "0190f3c2-0000-7000-8000-000000000001".into(), cwd: "/w".into() }));
    for bad in [br#"{"type":"take","sessionId":"--help","cwd":"/w"}"#.as_slice(), br#"{"type":"take","cwd":"/w"}"#] {
        let Err(HostAnswer::Refused { code, .. }) = open(&seal(bad)) else { panic!("opened") };
        assert_eq!(code, "bad_command");
    }
}
