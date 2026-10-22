//! The joining side of a pairing (R-E2E-3; engineering/devices.md in Canon
//! → Adding a device), run by krowk-client so every client holds B to the
//! same rules (R-CLIENT-1): it finishes when A posts the entry, and on any
//! failure it ends the pairing, never retries a step, a read or the code,
//! and keeps nothing.
//!
//! The mailbox here plays A in-process with a real `PairA`: each of B's
//! steps is answered as A would, the answer shows on the read after next,
//! and every call is recorded so a test can say what B sent and how often.

use krowk_api::sync::{Pairing, PairingStep};
use krowk_api::Error;
use krowk_client::device_chain::{Chain, Change, Kind, Subject};
use krowk_client::e2e::{self, DeviceKey, SigningKey};
use krowk_client::keystore::{Keystore, MemoryStorage};
use krowk_client::pairing::{AwaitAck, AwaitConfirm, Binding, NewDevice, PairA, PairingCode, PeerKind};
use krowk_client::pairing_join::{join, JoinFailed, Joined, Mailbox, Timing};
use krowk_client::pairing_payload::seal;
use krowk_client::user_key::UserKeys;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

const USER: &str = "0192f5a0-7e1c-7cc3-9d1a-5b6f00000001";
const TIME: u64 = 1_790_000_000;

/// Short enough that a test that waits out a deadline is quick; `pause`
/// never sleeps.
const TIMING: Timing = Timing { poll: Duration::from_millis(1), wait: Duration::from_millis(200), posted: Duration::from_millis(50) };

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Step(PairingStep),
    Read,
    End,
    Claim,
    Pause,
}

/// A's half as the mailbox carries it along.
enum A {
    Hello(PairA),
    Confirm(AwaitConfirm),
    Ack(AwaitAck),
    Over,
}

/// A, with its one-device list and the user key, before B joins.
struct Side {
    key: DeviceKey,
    signing: SigningKey,
    chain: Chain,
    entries: Vec<krowk_client::device_chain::SignedEntry>,
    keys: UserKeys,
}

fn side() -> Side {
    let (key, signing) = (DeviceKey::generate(), SigningKey::generate());
    let subject = Subject { kind: Kind::Device, name: "a-desktop".into(), os: "Linux".into(), device: key.public(), signing: signing.public() };
    let (chain, first) = Chain::start(subject, &signing, None, TIME).unwrap();
    let keys = UserKeys::new(first.newest, first.links).unwrap();
    Side { key, signing, chain, entries: first.entries, keys }
}

/// What the fake does other than play A straight.
#[derive(Default)]
struct Faults {
    /// The read with this 1-based number fails, lost on the way.
    read_error_at: Option<usize>,
    /// A's reply with a byte flipped in flight.
    tamper_reply: bool,
    /// A seals a chain whose last entry adds another device.
    adds_another: bool,
    /// The ack's answer: lands but is lost (0), or is refused (409).
    ack_status: Option<u16>,
    /// A never posts the entry, so no claim succeeds.
    never_posts: bool,
    /// The person cancels once the first claim has been refused.
    cancelled_at_claim: bool,
}

struct FakeMailbox {
    side: Side,
    a: RefCell<A>,
    me: NewDevice,
    pairing: RefCell<Pairing>,
    /// A's answer, published on the read after the one that comes next, so
    /// every wait reads, pauses and reads again.
    pending: RefCell<Option<(&'static str, Vec<u8>)>>,
    posted: Cell<bool>,
    calls: RefCell<Vec<Call>>,
    faults: Faults,
}

fn err(status: u16, code: &str) -> Error {
    let mut body = std::collections::BTreeMap::new();
    body.insert("error".into(), serde_json::json!(code));
    Error { status, body }
}

impl FakeMailbox {
    fn new(side: Side, a: PairA, me: NewDevice, faults: Faults) -> FakeMailbox {
        let pairing = Pairing { id: "pairing-1".into(), state: "open".into(), ..Default::default() };
        FakeMailbox { side, a: RefCell::new(A::Hello(a)), me, pairing: RefCell::new(pairing), pending: RefCell::new(None), posted: Cell::new(false), calls: RefCell::new(vec![]), faults }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn sent(&self, step: PairingStep) -> usize {
        self.calls().iter().filter(|c| **c == Call::Step(step)).count()
    }

    fn count(&self, call: Call) -> usize {
        self.calls().iter().filter(|c| **c == call).count()
    }

    fn die(&self) {
        *self.a.borrow_mut() = A::Over;
        self.pairing.borrow_mut().state = "dead".into();
    }

    /// The add A seals: B's subject on the end of A's chain, or another
    /// device's when the fault says so.
    fn sealed(&self) -> Vec<u8> {
        let s = &self.side;
        let other = (DeviceKey::generate().public(), SigningKey::generate().public());
        let (device, signing) = if self.faults.adds_another { other } else { (self.me.device, self.me.signing) };
        let subject = Subject { kind: Kind::Device, name: self.me.name.clone(), os: self.me.os.clone(), device, signing };
        let id = subject.id();
        let (next, batch) = s.chain.batch(s.keys.newest(), vec![Change::Add(subject)], s.key.id(), &s.signing, TIME).unwrap();
        let wrapped = batch.wraps.iter().find(|(d, _)| *d == id).unwrap().1.clone();
        let all = [s.entries.clone(), batch.entries].concat();
        seal(&all, &s.keys, next.generation(), &wrapped, &s.signing.public()).to_vec()
    }
}

impl Mailbox for FakeMailbox {
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), Error> {
        self.calls.borrow_mut().push(Call::Step(step));
        let a = std::mem::replace(&mut *self.a.borrow_mut(), A::Over);
        let field = match step {
            PairingStep::Join => "joiner_message",
            PairingStep::Confirmation => "joiner_confirmation",
            PairingStep::Acknowledgement => "joiner_ack",
            _ => panic!("B sent A's step {step:?}"),
        };
        match (step, a) {
            (PairingStep::Join, A::Hello(a)) => {
                let (next, spake) = a.receive_hello(message).unwrap();
                *self.a.borrow_mut() = A::Confirm(next);
                *self.pending.borrow_mut() = Some(("initiator_message", spake));
            }
            (PairingStep::Confirmation, A::Confirm(a)) => match a.receive_confirm(message) {
                Ok(confirmed) => {
                    let (next, mut reply) = confirmed.approve(&self.sealed()).unwrap();
                    if self.faults.tamper_reply {
                        let last = reply.len() - 1;
                        reply[last] ^= 1;
                    }
                    *self.a.borrow_mut() = A::Ack(next);
                    *self.pending.borrow_mut() = Some(("sealed_reply", reply));
                }
                // A's check failed: A ends the pairing, and so the registry.
                Err(_) => self.die(),
            },
            (PairingStep::Acknowledgement, A::Ack(a)) => {
                if self.faults.ack_status == Some(409) {
                    self.die();
                    return Err(err(409, "pairing_out_of_turn"));
                }
                a.receive_ack(message).unwrap();
                if !self.faults.never_posts {
                    self.posted.set(true);
                }
            }
            // Out of turn: the registry ends the pairing.
            _ => {
                self.die();
                return Err(err(409, "pairing_out_of_turn"));
            }
        }
        let mut p = self.pairing.borrow_mut();
        let hex = e2e::hex(message);
        match field {
            "joiner_message" => p.joiner_message = hex,
            "joiner_confirmation" => p.joiner_confirmation = hex,
            _ => p.joiner_ack = hex,
        }
        match (step, self.faults.ack_status) {
            (PairingStep::Acknowledgement, Some(0)) => Err(err(0, "network_error")),
            _ => Ok(()),
        }
    }

    fn read(&self) -> Result<Pairing, Error> {
        self.calls.borrow_mut().push(Call::Read);
        if self.faults.read_error_at == Some(self.count(Call::Read)) {
            return Err(err(0, "network_error"));
        }
        let seen = self.pairing.borrow().clone();
        if let Some((field, blob)) = self.pending.borrow_mut().take() {
            let mut p = self.pairing.borrow_mut();
            match field {
                "initiator_message" => p.initiator_message = e2e::hex(&blob),
                _ => p.sealed_reply = e2e::hex(&blob),
            }
        }
        Ok(seen)
    }

    fn end(&self) {
        self.calls.borrow_mut().push(Call::End);
        self.die();
    }

    fn claim_device(&self) -> Result<(), Error> {
        self.calls.borrow_mut().push(Call::Claim);
        if self.posted.get() { Ok(()) } else { Err(err(403, "device_not_listed")) }
    }

    fn pause(&self, _: Duration) {
        self.calls.borrow_mut().push(Call::Pause);
    }

    fn cancelled(&self) -> bool {
        self.faults.cancelled_at_claim && self.count(Call::Claim) > 0
    }
}

/// B: a fresh keystore in memory, and the device as it introduces itself.
struct B {
    store: Keystore,
    device: DeviceKey,
    me: NewDevice,
}

fn b() -> B {
    let store = Keystore::with(Arc::new(MemoryStorage::new()));
    let device = store.device_key().unwrap();
    let signing = store.signing_key().unwrap();
    let me = NewDevice { device: device.public(), signing: signing.public(), name: "b-macbook".into(), os: "macOS".into() };
    B { store, device, me }
}

/// The code as B's person types it off A's screen: a new value, since a
/// `PairingCode` is neither `Clone` nor reusable.
fn typed(a: &PairA) -> PairingCode {
    PairingCode::parse(&a.code().to_string()).unwrap()
}

/// One join against an A that plays it with `faults`; `wrong` types another
/// code than A shows.
fn run(faults: Faults, wrong: bool) -> (Result<Joined, JoinFailed>, FakeMailbox, B, Chain) {
    run_as(b(), faults, wrong)
}

fn run_as(b: B, faults: Faults, wrong: bool) -> (Result<Joined, JoinFailed>, FakeMailbox, B, Chain) {
    let side = side();
    let a = PairA::new(PeerKind::SamePersonDevice, USER, side.key.id()).unwrap();
    let code = if wrong { typed(&PairA::new(PeerKind::SamePersonDevice, USER, side.key.id()).unwrap()) } else { typed(&a) };
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: USER.into(), a_device: side.key.id(), b_device: b.device.id() };
    let before = side.chain.clone();
    let mailbox = FakeMailbox::new(side, a, b.me.clone(), faults);
    let result = join(&mailbox, &b.store, &b.device, binding, code, b.me.clone(), &TIMING);
    (result, mailbox, b, before)
}

fn kept_nothing(b: &B) {
    assert!(b.store.user_keys().unwrap().is_none(), "no user key is kept");
    assert!(b.store.device_list().unwrap().is_none(), "no device list is kept");
}

fn each_step_once(m: &FakeMailbox) {
    for step in [PairingStep::Join, PairingStep::Confirmation, PairingStep::Acknowledgement] {
        assert!(m.sent(step) <= 1, "{step:?} sent {} times", m.sent(step));
    }
}

#[test]
fn r_e2e_3_a_join_keeps_the_keys_and_the_pin_once_a_has_posted_the_entry() {
    let (result, m, b, before) = run(Faults::default(), false);
    let joined = result.unwrap();
    let pinned = b.store.device_list().unwrap().expect("the device list is kept");
    assert_eq!(joined, Joined { seq: pinned.head().seq, name: "b-macbook".into() });
    assert_eq!(joined.seq, before.head().seq + 1, "the head is A's add of this device");
    assert!(pinned.devices().iter().any(|d| d.id() == b.device.id()));
    assert_eq!(b.store.user_keys().unwrap().expect("the user key is kept").newest(), m.side.keys.newest());
    assert_eq!(m.count(Call::End), 0, "a join that finished is not ended");
    for step in [PairingStep::Join, PairingStep::Confirmation, PairingStep::Acknowledgement] {
        assert_eq!(m.sent(step), 1, "{step:?} sent once");
    }
    // Each of A's messages took a read, a pause and a read.
    assert_eq!(m.count(Call::Read), 4);
    assert_eq!(m.count(Call::Claim), 1);
}

#[test]
fn r_e2e_3_a_wrong_code_ends_the_pairing_and_keeps_nothing() {
    let (result, m, b, _) = run(Faults::default(), true);
    assert_eq!(result, Err(JoinFailed::CodeUsedUp));
    assert_eq!(m.count(Call::End), 1);
    assert_eq!(m.sent(PairingStep::Join), 1, "the code starts one pairing, once");
    assert_eq!(m.sent(PairingStep::Acknowledgement), 0);
    assert_eq!(m.count(Call::Claim), 0);
    each_step_once(&m);
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_a_read_lost_mid_wait_ends_the_join_without_reading_again() {
    // Read 2 follows the first wait's pause; read 4, the second's.
    for at in [2, 4] {
        let (result, m, b, _) = run(Faults { read_error_at: Some(at), ..Faults::default() }, false);
        assert_eq!(result, Err(JoinFailed::CodeUsedUp));
        assert_eq!(m.count(Call::Read), at, "no read after the one that was lost");
        assert_eq!(m.calls().last(), Some(&Call::End));
        assert_eq!(m.sent(PairingStep::Join), 1);
        assert_eq!(m.sent(PairingStep::Confirmation), usize::from(at == 4));
        kept_nothing(&b);
    }
}

#[test]
fn r_e2e_3_a_join_that_fails_before_the_reply_checks_leaves_the_keys_held_before_it() {
    // A user key and list an earlier pairing left go only on A's
    // authenticated word, never because a pairing failed.
    let b = b();
    let earlier = side();
    b.store.save_user_keys(&earlier.keys).unwrap();
    b.store.save_device_list(&earlier.entries).unwrap();
    for (faults, wrong) in [(Faults::default(), true), (Faults { tamper_reply: true, ..Faults::default() }, false)] {
        let (result, m, held, _) = run_as(B { store: b.store.clone(), device: b.store.device_key().unwrap(), me: b.me.clone() }, faults, wrong);
        assert_eq!(result, Err(JoinFailed::CodeUsedUp));
        assert_eq!(m.count(Call::End), 1);
        assert_eq!(held.store.user_keys().unwrap().expect("the held user key stays").newest(), earlier.keys.newest());
        assert_eq!(held.store.device_list().unwrap().expect("the held list stays"), earlier.chain);
    }
}

#[test]
fn r_e2e_3_a_tampered_reply_ends_the_pairing_and_keeps_nothing() {
    let (result, m, b, _) = run(Faults { tamper_reply: true, ..Faults::default() }, false);
    assert_eq!(result, Err(JoinFailed::CodeUsedUp));
    assert_eq!(m.count(Call::End), 1);
    assert_eq!(m.sent(PairingStep::Acknowledgement), 0, "a reply that did not open is never acked");
    each_step_once(&m);
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_a_reply_that_adds_another_device_ends_the_pairing_and_keeps_nothing() {
    let (result, m, b, _) = run(Faults { adds_another: true, ..Faults::default() }, false);
    assert_eq!(result, Err(JoinFailed::CodeUsedUp));
    assert_eq!(m.count(Call::End), 1);
    assert_eq!(m.sent(PairingStep::Acknowledgement), 0, "a reply that did not check is never acked");
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_an_ack_whose_answer_was_lost_still_joins() {
    let (result, m, b, _) = run(Faults { ack_status: Some(0), ..Faults::default() }, false);
    let joined = result.unwrap();
    assert_eq!(joined.seq, b.store.device_list().unwrap().unwrap().head().seq);
    assert_eq!(m.sent(PairingStep::Acknowledgement), 1, "the ack is not sent again");
    assert_eq!(m.count(Call::End), 0);
    assert!(b.store.user_keys().unwrap().is_some());
}

#[test]
fn r_e2e_3_an_ack_the_registry_refuses_forgets_the_keys() {
    let (result, m, b, _) = run(Faults { ack_status: Some(409), ..Faults::default() }, false);
    assert_eq!(result, Err(JoinFailed::CodeUsedUp));
    assert_eq!(m.sent(PairingStep::Acknowledgement), 1, "the ack is not sent again");
    assert_eq!(m.count(Call::End), 1);
    assert_eq!(m.count(Call::Claim), 0);
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_an_entry_a_never_posts_forgets_the_keys_once_the_wait_is_over() {
    let (result, m, b, _) = run(Faults { never_posts: true, ..Faults::default() }, false);
    assert_eq!(result, Err(JoinFailed::NotAdded));
    assert!(m.count(Call::Claim) > 1, "the claim is asked until the wait is over");
    assert_eq!(m.calls().last(), Some(&Call::End));
    assert_eq!(m.count(Call::End), 1);
    each_step_once(&m);
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_a_join_cancelled_while_it_claims_stops_claiming_and_forgets_the_keys() {
    let b = b();
    let side = side();
    let a = PairA::new(PeerKind::SamePersonDevice, USER, side.key.id()).unwrap();
    let code = typed(&a);
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: USER.into(), a_device: side.key.id(), b_device: b.device.id() };
    let m = FakeMailbox::new(side, a, b.me.clone(), Faults { never_posts: true, cancelled_at_claim: true, ..Faults::default() });
    // A wait long enough that only the cancel can have ended it.
    let timing = Timing { posted: Duration::from_secs(600), ..TIMING };
    let result = join(&m, &b.store, &b.device, binding, code, b.me.clone(), &timing);
    assert_eq!(result, Err(JoinFailed::NotAdded));
    assert_eq!(m.count(Call::Claim), 1, "no claim after the cancel");
    assert_eq!(m.calls().last(), Some(&Call::End));
    kept_nothing(&b);
}

#[test]
fn r_e2e_3_the_default_timing_is_the_terminals() {
    assert_eq!(Timing::default(), Timing { poll: Duration::from_millis(500), wait: Duration::from_secs(600), posted: Duration::from_secs(60) });
}
