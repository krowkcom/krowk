//! A command one device sends a host, and its answer, sealed to the
//! person's devices and bound to the command's id and both devices
//! (R-HOST-4, R-E2E-1): what opens, what refuses, and what is dropped
//! unread. The device lists are built as `presence.rs` builds them.

use gruntas_client::device_chain::{Action, Chain, Entry, Kind, SignedEntry, Subject};
use gruntas_client::e2e::{self, hex, DeviceId, DeviceKey, Label, SigningKey};
use gruntas_client::host_command::{self, HostAnswer, HostCommand, JobRef, RunRefusal, RunTask};
use gruntas_client::protocol::{BudgetLimits, ModelRef, PermissionMode};
use gruntas_client::recovery::RecoveryKit;
use gruntas_client::user_key::{UserKey, UserKeys};

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
    HostCommand::Start { cwd: "/home/someone/secret-project".into(), model: ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() }, prompt: "fix the secret-flaky test".into(), sending_session: None }
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

/// SL06 (D5): a ping, for a machine or a session, and its pong open on the
/// other side; a session id no host would hold is refused as it opens.
#[test]
fn sl06_a_ping_and_its_pong_open_on_the_other_side() {
    use gruntas_client::host_command::SessionPing;
    use gruntas_client::status::Health;
    let w = world();
    let c = chain(&w);
    for ping in [HostCommand::Ping { session: None }, HostCommand::Ping { session: Some("0190f3c2-0000-7000-8000-000000000001".into()) }] {
        let sealed = host_command::seal_command(&ping, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
        assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(ping));
    }
    for bad in ["", "a\nb", &"x".repeat(host_command::MAX_SESSION_ID + 1)] {
        let sealed = host_command::seal_command(&HostCommand::Ping { session: Some(bad.into()) }, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
        assert!(matches!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Err(HostAnswer::Refused { code, .. }) if code == "bad_command"), "{bad:?}");
    }
    let session = SessionPing { health: Health::Serving, last_activity: Some("2026-11-23T11:59:00Z".into()), at: "2026-11-23T12:00:30Z".into() };
    assert_eq!(session.quiet_for(), Some(jiff::SignedDuration::from_secs(90)), "by the host's clock alone");
    for pong in [HostAnswer::Pong { session: None }, HostAnswer::Pong { session: Some(session) }] {
        let sealed = host_command::seal_answer(&pong, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
        assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), pong);
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
    assert_eq!(opened, HostCommand::Start { cwd: "/w".into(), model: ModelRef { instance: "anthropic".into(), model: "m".into() }, prompt: "hi".into(), sending_session: None });
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
    let long = HostCommand::Start { cwd, model, prompt: "x".repeat(host_command::MAX_SEALED), sending_session: None };
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
    let take = HostCommand::Take { session_id: "0190f3c2-0000-7000-8000-000000000001".into(), cwd: "/home/someone/clone".into(), origin: None };
    let sealed = host_command::seal_command(&take, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(take.clone()));
    let taken = HostAnswer::Taken { session_id: "0190f3c2-0000-7000-8000-000000000001".into() };
    let sealed = host_command::seal_answer(&taken, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), taken);

    let seal = |json: &[u8]| e2e::seal_to_person(&keys(1), 1, Label::HostCommand, &host_command::binding(ID, &w.phone.id(), &w.host.id()), json).unwrap();
    let open = |sealed: &[u8]| host_command::open_command(sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap();
    let extra = br#"{"type":"take","sessionId":"0190f3c2-0000-7000-8000-000000000001","cwd":"/w","permissionMode":"unhinged"}"#;
    assert_eq!(open(&seal(extra)), Ok(HostCommand::Take { session_id: "0190f3c2-0000-7000-8000-000000000001".into(), cwd: "/w".into(), origin: None }));
    for bad in [br#"{"type":"take","sessionId":"--help","cwd":"/w"}"#.as_slice(), br#"{"type":"take","cwd":"/w"}"#] {
        let Err(HostAnswer::Refused { code, .. }) = open(&seal(bad)) else { panic!("opened") };
        assert_eq!(code, "bad_command");
    }
}

/// A `run` from `from`, its job named `job_key`.
fn run(from: &DeviceId) -> HostCommand {
    HostCommand::Run(RunTask {
        origin: "github.com/someone/secret-project".into(),
        model: ModelRef { instance: "claude:work".into(), model: "claude-opus-5-5".into() },
        job: JobRef { session: "0190f3c2-0000-7000-8000-000000000001".into(), transport: "art_0123456789abcdef".into(), key: "ab".repeat(32) },
        budget: BudgetLimits { max_usd: Some(2.5), max_tokens: None },
        requested_by: from.to_string(),
        sending_session: None,
        permission_mode: Some(PermissionMode::Plan),
        not_after_ms: 1_790_000_030_000,
    })
}

/// R-ALLOC-3: a `run` opens on the host as sent, with the wire names canon
/// documents, and each answer it gets opens on the supervisor's machine.
#[test]
fn r_alloc_3_a_run_and_its_answers_open_on_the_other_side() {
    let w = world();
    let c = chain(&w);
    let sealed = host_command::seal_command(&run(&w.phone.id()), ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(run(&w.phone.id())));
    assert!(sealed.len() <= host_command::MAX_SEALED, "a run is far within the 16 KB a command may seal to");
    for secret in ["secret-project", "claude:work", &"ab".repeat(32)] {
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} is in the sealed bytes");
    }
    let me = w.phone.id().to_string();
    let wire = serde_json::to_value(run(&w.phone.id())).unwrap();
    assert_eq!(wire, serde_json::json!({"type": "run", "origin": "github.com/someone/secret-project", "model": {"instance": "claude:work", "model": "claude-opus-5-5"}, "job": {"session": "0190f3c2-0000-7000-8000-000000000001", "transport": "art_0123456789abcdef", "key": "ab".repeat(32)}, "budget": {"maxUsd": 2.5}, "requestedBy": me, "permissionMode": "plan", "notAfterMs": 1_790_000_030_000i64}));
    let bare = serde_json::json!({"type": "run", "origin": "o", "model": {"instance": "anthropic", "model": "m"}, "job": {"session": "0190f3c2-0000-7000-8000-000000000001", "transport": "art_1", "key": "ab".repeat(32)}, "requestedBy": me, "notAfterMs": 1});
    let HostCommand::Run(t) = serde_json::from_value(bare).unwrap() else { panic!("not a run") };
    assert_eq!((t.budget, t.permission_mode), (BudgetLimits::default(), None), "neither budget nor mode is needed");

    let mut answers = vec![HostAnswer::Started { session_id: "0190f3c2-0000-7000-8000-000000000002".into() }];
    answers.extend(RunRefusal::ALL.map(|r| r.answer("a fix line")));
    for answer in answers {
        let sealed = host_command::seal_answer(&answer, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
        assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), answer);
    }
}

/// R-ALLOC-3: the refusal codes are the spec's, each read back as itself.
#[test]
fn r_alloc_3_the_refusal_codes_are_the_specs() {
    let codes: Vec<&str> = RunRefusal::ALL.iter().map(|r| r.code()).collect();
    assert_eq!(codes, ["no_clone", "no_model", "no_slot", "not_accepting", "machine_limit", "bad_job"]);
    for r in RunRefusal::ALL {
        assert_eq!(RunRefusal::of(r.code()), Some(r));
    }
    assert_eq!(RunRefusal::of("unknown_command"), None);
}

/// R-ALLOC-3: a job named as no stored job is is answered `bad_job`, and a
/// `run` that says it is from another device than its sender's is refused.
#[test]
fn r_alloc_3_a_run_is_refused_for_a_bad_job_or_another_devices_name() {
    let w = world();
    let seal = |c: &HostCommand| host_command::seal_command(c, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    let open = |sealed: &[u8]| host_command::open_command(sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &chain(&w)).unwrap();
    let HostCommand::Run(good) = run(&w.phone.id()) else { unreachable!() };
    for job in [JobRef { session: "--help".into(), ..good.job.clone() }, JobRef { transport: "../x".into(), ..good.job.clone() }, JobRef { key: "zz".repeat(32), ..good.job.clone() }] {
        let Err(HostAnswer::Refused { code, .. }) = open(&seal(&HostCommand::Run(RunTask { job, ..good.clone() }))) else { panic!("opened") };
        assert_eq!(code, "bad_job");
    }
    let Err(HostAnswer::Refused { code, .. }) = open(&seal(&run(&w.tablet.id()))) else { panic!("opened") };
    assert_eq!(code, "bad_command", "a run from the phone naming the tablet");
}

/// Slice 05 (D13): a `run`'s mode only narrows the host's: the stricter
/// of the two, in the documented order, either way round.
#[test]
fn alloc_05_a_runs_mode_only_narrows_the_hosts() {
    use PermissionMode::*;
    let order = [Plan, Default, AcceptEdits, BypassPermissions, Unhinged];
    for (i, a) in order.iter().enumerate() {
        for (j, b) in order.iter().enumerate() {
            let want = order[i.min(j)];
            assert_eq!(host_command::stricter(*a, *b), want, "{a:?} and {b:?}");
        }
    }
}

/// The shapes a transport is held to wherever it is named.
#[test]
fn r_alloc_3_a_transport_is_held_to_its_shape() {
    assert!(host_command::slug_shaped("art_0123") && !host_command::slug_shaped("../x") && !host_command::slug_shaped(""));
    assert!(host_command::key_shaped(&"ab".repeat(32)) && !host_command::key_shaped(&"zz".repeat(32)) && !host_command::key_shaped("ab"));
}

/// R-SEC-3 (D4, D7): a `start` and a `run` may name the session that sent
/// them, a `take` its origin; each opens without them, as an older sender
/// wrote it.
#[test]
fn r_sec_3_a_host_command_may_name_its_sending_session_and_origin() {
    use gruntas_client::protocol::{Origin, OriginKind};
    let w = world();
    let c = chain(&w);
    let opened = |json: &str| {
        let sealed = e2e::seal_to_person(&keys(1), 1, Label::HostCommand, &host_command::binding(ID, &w.phone.id(), &w.host.id()), json.as_bytes()).unwrap();
        match host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c) {
            Ok(Ok(c)) => c,
            other => panic!("{json}: {other:?}"),
        }
    };
    let s = "0190f3c2-0000-7000-8000-0000000000aa";
    let start = format!(r#"{{"type":"start","cwd":"/w","model":{{"instance":"anthropic","model":"m"}},"prompt":"hi","sendingSession":"{s}"}}"#);
    assert!(matches!(opened(&start), HostCommand::Start { sending_session: Some(ref x), .. } if x == s));
    let take = format!(r#"{{"type":"take","sessionId":"{s}","cwd":"/w","origin":{{"kind":"agent","sessionId":"{s}","device":"laptop"}}}}"#);
    let want = Origin { kind: OriginKind::Agent, session_id: Some(s.into()), device: Some("laptop".into()), attendance: None };
    assert!(matches!(opened(&take), HostCommand::Take { origin: Some(ref o), .. } if *o == want));
    let HostCommand::Run(mut task) = run(&w.phone.id()) else { unreachable!() };
    task.sending_session = Some(s.into());
    let json = serde_json::to_value(HostCommand::Run(task.clone())).unwrap();
    assert_eq!(json["sendingSession"], s);
    assert!(matches!(opened(&json.to_string()), HostCommand::Run(t) if t.sending_session.as_deref() == Some(s)));
    // Absent: a `run` of an older supervisor opens, and writes no field.
    task.sending_session = None;
    assert!(serde_json::to_value(HostCommand::Run(task)).unwrap().get("sendingSession").is_none());
}

fn agent() -> gruntas_client::protocol::Origin {
    use gruntas_client::protocol::{Origin, OriginKind};
    Origin { kind: OriginKind::Agent, session_id: Some("0190f3c2-0000-7000-8000-0000000000aa".into()), device: Some("laptop".into()), attendance: None }
}

const GOD_SESSION: &str = "0190f3c2-0000-7000-8000-000000000001";

/// GA01 (D1–D5): the god agent's four commands open on the host as sent,
/// with the wire names the tools and older hosts see.
#[test]
fn ga01_the_god_agents_commands_open_on_the_other_side() {
    let w = world();
    let c = chain(&w);
    let commands = [
        HostCommand::Read { session: GOD_SESSION.into(), page: None },
        HostCommand::Read { session: GOD_SESSION.into(), page: Some("before:4096".into()) },
        HostCommand::Changes { since: "2026-11-24T10:00:00Z".into() },
        HostCommand::Message { session: GOD_SESSION.into(), text: "rebase first".into(), origin: agent() },
        HostCommand::Interrupt { session: GOD_SESSION.into(), origin: agent() },
    ];
    for command in commands {
        let sealed = host_command::seal_command(&command, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
        assert_eq!(host_command::open_command(&sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &c).unwrap(), Ok(command));
    }
    let wire = serde_json::to_value(HostCommand::Message { session: GOD_SESSION.into(), text: "t".into(), origin: agent() }).unwrap();
    assert_eq!(wire, serde_json::json!({"type": "message", "session": GOD_SESSION, "text": "t", "origin": {"kind": "agent", "sessionId": "0190f3c2-0000-7000-8000-0000000000aa", "device": "laptop"}}));
    assert_eq!(serde_json::to_value(HostCommand::Read { session: GOD_SESSION.into(), page: None }).unwrap(), serde_json::json!({"type": "read", "session": GOD_SESSION}));
}

/// GA01: what a host takes of the god agent's commands: a session id, a
/// time, text within bounds, and only an agent's steer.
#[test]
fn ga01_the_god_agents_commands_are_held_to_their_shapes() {
    use gruntas_client::protocol::OriginKind;
    let w = world();
    let seal = |c: &HostCommand| host_command::seal_command(c, ID, &w.phone.id(), &w.host.id(), &keys(1), 1).unwrap();
    let open = |sealed: &[u8]| host_command::open_command(sealed, ID, &w.phone.id(), &w.host.id(), &keys(1), &chain(&w)).unwrap();
    let person = gruntas_client::protocol::Origin { kind: OriginKind::Person, ..agent() };
    let bad = [
        HostCommand::Read { session: "../../etc/passwd".into(), page: None },
        HostCommand::Read { session: GOD_SESSION.into(), page: Some("x".repeat(host_command::MAX_CURSOR + 1)) },
        HostCommand::Changes { since: "yesterday".into() },
        HostCommand::Message { session: GOD_SESSION.into(), text: " ".into(), origin: agent() },
        HostCommand::Message { session: GOD_SESSION.into(), text: "x".repeat(host_command::MAX_STEER_TEXT + 1), origin: agent() },
        HostCommand::Message { session: GOD_SESSION.into(), text: "hi".into(), origin: person.clone() },
        HostCommand::Interrupt { session: GOD_SESSION.into(), origin: person },
    ];
    for c in bad {
        assert!(matches!(open(&seal(&c)), Err(HostAnswer::Refused { ref code, .. }) if code == "bad_command"), "{c:?}");
    }
}

/// GA01 (D3, R-GOD-8): each answer opens on the asking side; one too big
/// for the 4 KB answer goes in the answer record, the answer pointing at
/// it, and the record opens to the whole answer, bound as the answer is.
#[test]
fn ga01_an_answer_too_big_goes_in_the_answer_record() {
    use gruntas_client::host_command::{Change, ChangesSince, TranscriptPage};
    let w = world();
    let c = chain(&w);
    let small = [
        HostAnswer::Delivered { session: GOD_SESSION.into() },
        HostAnswer::Changes(ChangesSince { since: "2026-11-24T10:00:00Z".into(), at: "2026-11-24T10:05:00Z".into(), changes: vec![Change { session: GOD_SESSION.into(), at: "2026-11-24T10:01:00Z".into(), state: Some(gruntas_client::status::TaskState::Completed), summary: Some("PR opened".into()) }] }),
        HostAnswer::refused(host_command::refusal::NOTHING_RUNNING, "nothing is running"),
    ];
    for answer in small {
        let (sealed, record) = host_command::seal_answer_or_record(&answer, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
        assert!(record.is_none(), "{answer:?}");
        assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), answer);
    }
    let page = HostAnswer::Page(TranscriptPage { session: GOD_SESSION.into(), text: "é".repeat(6 * 1024), next: Some("before:12288".into()) });
    let (sealed, record) = host_command::seal_answer_or_record(&page, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).unwrap();
    assert_eq!(host_command::open_answer(&sealed, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), HostAnswer::Recorded);
    let record = record.expect("a record");
    assert!(record.len() <= host_command::MAX_RECORD);
    assert_eq!(host_command::open_record(&record, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).unwrap(), page);
    // Bound to the command and its devices, and no answer itself.
    assert!(host_command::open_record(&record, OTHER_ID, &w.host.id(), &w.phone.id(), &keys(1), &c).is_err());
    assert!(host_command::open_answer(&record, ID, &w.host.id(), &w.phone.id(), &keys(1), &c).is_err());
    // A record holds an answer, never a pointer; past 64 KB it is refused.
    assert!(host_command::seal_record(&HostAnswer::Recorded, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).is_err());
    let huge = HostAnswer::Page(TranscriptPage { session: GOD_SESSION.into(), text: "x".repeat(host_command::MAX_RECORD), next: None });
    assert!(host_command::seal_answer_or_record(&huge, ID, &w.host.id(), &w.phone.id(), &keys(1), 1).is_err());
}

/// GA01 (D8): an older host refuses a command it does not know as
/// `unknown_command`, which the tools say as "that machine needs updating";
/// any other refusal is its own fix line.
#[test]
fn ga01_an_older_hosts_refusal_reads_as_needs_updating() {
    let older = HostAnswer::refused("unknown_command", "this host does not know the command \"read\" — upgrade gruntas on it");
    assert!(older.needs_updating());
    assert!(older.refusal_line("studio").unwrap().starts_with("studio: that machine needs updating"));
    let refused = HostAnswer::refused(host_command::refusal::CANT_STEER, "can't steer this session yet: it runs in Codex");
    assert!(!refused.needs_updating());
    assert_eq!(refused.refusal_line("studio").unwrap(), "studio: can't steer this session yet: it runs in Codex");
    assert_eq!(HostAnswer::Delivered { session: GOD_SESSION.into() }.refusal_line("studio"), None);
}
