//! Pairing by a short code (ticket D2, engineering/devices.md in Canon →
//! Adding a device): the code, the flow both ways, each way it must fail,
//! and the SPAKE2 crate held to the vectors magic-wormhole interoperates
//! with.

use krowk_client::e2e::{DeviceKey, SigningKey};
use krowk_client::pairing::{
    ALPHABET, AwaitAck, AwaitConfirm, AwaitReply, Binding, NewDevice, PairA, PairB, Paired, PairingCode, PeerKind,
};

const USER: &str = "0192f5a0-7e1c-7cc3-9d1a-5b6f00000001";
const PAYLOAD: &[u8] = b"chain + wrap + signer key, opaque to pairing";

struct Sides {
    a_device: krowk_client::e2e::DeviceId,
    b: NewDevice,
}

fn sides() -> Sides {
    let b = NewDevice {
        device: DeviceKey::generate().public(),
        signing: SigningKey::generate().public(),
        name: "elvinas-macbook".into(),
        os: "macOS".into(),
    };
    Sides { a_device: DeviceKey::generate().id(), b }
}

/// The code as B's person types it off A's screen: a new value, since a
/// `PairingCode` is neither `Clone` nor reusable.
fn typed(a: &PairA) -> PairingCode {
    PairingCode::parse(&a.code().to_string()).unwrap()
}

fn binding(s: &Sides) -> Binding {
    Binding { kind: PeerKind::SamePersonDevice, user_id: USER.into(), a_device: s.a_device, b_device: s.b.id() }
}

/// Every message of one pairing, in order, and the states each side is
/// left in before the last step.
struct Run {
    hello: Vec<u8>,
    confirm: Vec<u8>,
    reply: Vec<u8>,
    ack: Vec<u8>,
}

fn full_run(s: &Sides) -> (Run, Paired, Vec<u8>) {
    let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let code = PairingCode::parse(&a.code().to_string().to_lowercase()).unwrap();
    let (b, hello) = PairB::start(binding(s), code, s.b.clone()).unwrap();
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (b, confirm) = b.receive_spake(&spake).unwrap();
    let a = a.receive_confirm(&confirm).unwrap();
    assert_eq!(a.device(), &s.b, "A shows exactly what B's MAC covered");
    let (a, reply) = a.approve(PAYLOAD).unwrap();
    let received = b.receive_reply(&reply).unwrap();
    assert_eq!(received.payload(), PAYLOAD);
    let (payload, ack) = received.acknowledge();
    let paired = a.receive_ack(&ack).unwrap();
    (Run { hello, confirm, reply, ack }, paired, payload.to_vec())
}

/// A and B up to B waiting for the reply and A holding B's confirmation.
fn to_confirm(s: &Sides) -> (AwaitConfirm, AwaitReply, Vec<u8>) {
    let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let (b, hello) = PairB::start(binding(s), typed(&a), s.b.clone()).unwrap();
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (b, confirm) = b.receive_spake(&spake).unwrap();
    (a, b, confirm)
}

fn to_ack(s: &Sides) -> (AwaitAck, Vec<u8>, Vec<u8>) {
    let (a, b, confirm) = to_confirm(s);
    let (a, reply) = a.receive_confirm(&confirm).unwrap().approve(PAYLOAD).unwrap();
    let (_, ack) = b.receive_reply(&reply).unwrap().acknowledge();
    (a, reply, ack)
}

fn flip(m: &[u8], i: usize) -> Vec<u8> {
    let mut m = m.to_vec();
    m[i] ^= 0x01;
    m
}

// ------------------------------------------------------------------ code

#[test]
fn d2_the_code_is_eight_symbols_of_crockford_base32_less_0_and_1() {
    assert_eq!(ALPHABET.len(), 30);
    for bad in b"01OILU" {
        assert!(!ALPHABET.contains(bad), "{}", *bad as char);
    }
    for _ in 0..200 {
        let c = PairingCode::generate();
        let shown = c.to_string();
        assert_eq!(shown.len(), 9);
        assert_eq!(&shown[4..5], "-");
        assert!(c.as_bytes().iter().all(|b| ALPHABET.contains(b)));
    }
}

#[test]
fn d2_the_code_draws_every_symbol() {
    // Rejection sampling over the OS source: in 400 codes (3200 symbols)
    // each of 30 symbols is all but certain to appear.
    let mut seen = [false; 256];
    for _ in 0..400 {
        for b in PairingCode::generate().as_bytes() {
            seen[*b as usize] = true;
        }
    }
    assert!(ALPHABET.iter().all(|b| seen[*b as usize]));
}

#[test]
fn d2_the_code_parser_ignores_case_spaces_and_dashes_only() {
    let want = PairingCode::parse("K7QF-9M3X").unwrap();
    assert_eq!(want.to_string(), "K7QF-9M3X");
    for typed in ["k7qf9m3x", "k7qf 9m3x", " K7QF - 9M3X ", "k-7-q-f-9-m-3-x", "K7QF-9M3X\n", "\tk7qf-9m3x\r\n"] {
        assert_eq!(PairingCode::parse(typed).map(|c| *c.as_bytes()), Some(*want.as_bytes()), "{typed:?}");
    }
    for typed in ["", "K7QF-9M3", "K7QF-9M3XX", "K7QF-9M30", "K7QF-9M31", "K7QF-9M3O", "K7QF-9M3I", "K7QF-9M3L", "K7QF-9M3U",
        "K7QF_9M3X", "K7QF\t9M3X", "K7QF\n9M3X", "K7QF.9M3X", "K7QF-9M3Ⅹ", "Ｋ7QF-9M3X"]
    {
        assert!(PairingCode::parse(typed).is_none(), "{typed:?}");
    }
    assert_eq!(format!("{want:?}"), "PairingCode(…)", "Debug never shows the code");
}

// ----------------------------------------------------------- the flow

#[test]
fn d2_a_pairing_completes_and_both_sides_agree() {
    let s = sides();
    let (_, paired, payload) = full_run(&s);
    assert_eq!(payload, PAYLOAD);
    assert_eq!(paired.device(), &s.b);
    assert_eq!(paired.binding(), &binding(&s));
}

/// One wrong guess ends it on both sides: A's state is consumed by the
/// failed confirmation (there is no value left to try the right code
/// against), and B never gets a reply that opens.
#[test]
fn d2_a_wrong_code_is_one_guess_then_dead_on_both_sides() {
    let s = sides();
    let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let mut wrong = a.code().to_string();
    let last = wrong.pop().unwrap();
    wrong.push(if last == 'Z' { 'Y' } else { 'Z' });
    let wrong = PairingCode::parse(&wrong).unwrap();
    let (b, hello) = PairB::start(binding(&s), wrong, s.b.clone()).unwrap();
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (b, confirm) = b.receive_spake(&spake).unwrap();
    assert!(a.receive_confirm(&confirm).is_err());
    // Nothing A could send opens at B: the best a registry has is a reply
    // from another pairing, and that is refused too.
    let (other, _, _) = full_run(&sides());
    assert!(b.receive_reply(&other.reply).is_err());
}

#[test]
fn d2_the_code_a_generates_is_new_every_time() {
    let s = sides();
    let a1 = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let a2 = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    assert_ne!(a1.code().as_bytes(), a2.code().as_bytes());
}

// ------------------------------------------------------ swapped identities

fn swapped_fails_at_a(bind_b: Binding, kind_a: PeerKind, user_a: &str, s: &Sides) {
    let a = PairA::new(kind_a, user_a, s.a_device).unwrap();
    let (b, hello) = PairB::start(bind_b, typed(&a), s.b.clone()).unwrap();
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (_, confirm) = b.receive_spake(&spake).unwrap();
    assert!(a.receive_confirm(&confirm).is_err());
}

/// Only kind 1 (same-person device) is defined; ticket 32 defines kind 2
/// with its own fields under the same label.
#[test]
fn d2_the_only_peer_kind_is_same_person_device() {
    let s = sides();
    let (_, paired, _) = full_run(&s);
    assert_eq!(paired.binding().kind, PeerKind::SamePersonDevice);
}

#[test]
fn d2_a_swapped_user_fails() {
    let s = sides();
    swapped_fails_at_a(Binding { user_id: "someone-else".into(), ..binding(&s) }, PeerKind::SamePersonDevice, USER, &s);
}

/// A registry that routes B to the wrong paired device (or lies to B about
/// A's id) makes the keys differ.
#[test]
fn d2_a_swapped_paired_device_fails() {
    let s = sides();
    swapped_fails_at_a(Binding { a_device: DeviceKey::generate().id(), ..binding(&s) }, PeerKind::SamePersonDevice, USER, &s);
}

/// B binds one device id and confirms another key: refused, even with the
/// code right, because A checks the hello's id is the confirmed key's.
#[test]
fn d2_a_new_device_whose_key_is_not_its_id_is_refused() {
    let s = sides();
    let other = DeviceKey::generate().public();
    assert!(PairB::start(Binding { b_device: other.id(), ..binding(&s) }, PairingCode::generate(), s.b.clone()).is_err());
    // At A: rewrite the hello's device id in flight.
    let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let (b, mut hello) = PairB::start(binding(&s), typed(&a), s.b.clone()).unwrap();
    hello[2..18].copy_from_slice(&other.id().0);
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (_, confirm) = b.receive_spake(&spake).unwrap();
    assert!(a.receive_confirm(&confirm).is_err());
}

#[test]
fn d2_a_name_or_os_a_terminal_would_misprint_is_refused() {
    let s = sides();
    for (name, os) in [("", "macOS"), ("x", ""), ("evil\x1b[2J", "macOS"), ("x", "mac\nOS"), ("a\u{202E}koobcam", "macOS"), ("elvinas-macbook\u{200B}", "macOS"),
        ("x", "mac\u{2028}OS"), ("soft\u{00AD}hyphen", "macOS"), ("tag\u{E0041}", "macOS"), ("x", "\u{FEFF}macOS"),
        (&"n".repeat(129) as &str, "macOS")]
    {
        let me = NewDevice { name: name.into(), os: os.into(), ..s.b.clone() };
        assert!(PairB::start(binding(&s), PairingCode::generate(), me).is_err(), "{name:?} {os:?}");
    }
}

// ------------------------------------------------------- tampering, each step

#[test]
fn d2_a_tampered_hello_fails() {
    let s = sides();
    // Every byte: the header, the device id, the SPAKE2 message.
    let probe = {
        let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
        PairB::start(binding(&s), typed(&a), s.b.clone()).unwrap().1.len()
    };
    for i in 0..probe {
        let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
        let (b, hello) = PairB::start(binding(&s), typed(&a), s.b.clone()).unwrap();
        let Ok((a, spake)) = a.receive_hello(&flip(&hello, i)) else { continue };
        let (_, confirm) = b.receive_spake(&spake).unwrap();
        assert!(a.receive_confirm(&confirm).is_err(), "byte {i}");
    }
}

#[test]
fn d2_a_tampered_spake_message_fails() {
    let s = sides();
    for i in 0..35 {
        let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
        let (b, hello) = PairB::start(binding(&s), typed(&a), s.b.clone()).unwrap();
        let (a, spake) = a.receive_hello(&hello).unwrap();
        let Ok((_, confirm)) = b.receive_spake(&flip(&spake, i)) else { continue };
        assert!(a.receive_confirm(&confirm).is_err(), "byte {i}");
    }
}

#[test]
fn d2_a_tampered_confirmation_fails() {
    let s = sides();
    let len = to_confirm(&s).2.len();
    for i in 0..len {
        let (a, _, confirm) = to_confirm(&s);
        assert!(a.receive_confirm(&flip(&confirm, i)).is_err(), "byte {i}");
    }
    let (a, _, mut confirm) = to_confirm(&s);
    confirm.push(0);
    assert!(a.receive_confirm(&confirm).is_err(), "a trailing byte");
}

#[test]
fn d2_a_tampered_reply_fails() {
    let s = sides();
    let len = to_ack(&s).1.len();
    for i in 0..len {
        let (a, b, confirm) = to_confirm(&s);
        let (_, reply) = a.receive_confirm(&confirm).unwrap().approve(PAYLOAD).unwrap();
        assert!(b.receive_reply(&flip(&reply, i)).is_err(), "byte {i}");
    }
    let (a, b, confirm) = to_confirm(&s);
    let (_, reply) = a.receive_confirm(&confirm).unwrap().approve(PAYLOAD).unwrap();
    assert!(b.receive_reply(&reply[..reply.len() - 1]).is_err(), "truncated");
}

#[test]
fn d2_a_tampered_ack_fails() {
    let s = sides();
    let len = to_ack(&s).2.len();
    for i in 0..len {
        let (a, _, ack) = to_ack(&s);
        assert!(a.receive_ack(&flip(&ack, i)).is_err(), "byte {i}");
    }
}

/// Each MAC names what it is: B's confirmation MAC presented as the ack
/// (same length, same key) is refused.
#[test]
fn d2_one_mac_cannot_stand_in_for_another() {
    let s = sides();
    let (a, b, confirm) = to_confirm(&s);
    let (a, _reply) = a.receive_confirm(&confirm).unwrap().approve(PAYLOAD).unwrap();
    drop(b);
    let forged = [&[1u8, 5][..], &confirm[confirm.len() - 32..]].concat();
    assert!(a.receive_ack(&forged).is_err());
}

// ----------------------------------------------------- replays across sessions

/// A reply recorded from one pairing, replayed into a second pairing with
/// the same devices, the same person and even the same code, opens nothing.
#[test]
fn d2_a_replayed_reply_is_refused() {
    let s = sides();
    let a = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let (b, hello) = PairB::start(binding(&s), typed(&a), s.b.clone()).unwrap();
    let shown = a.code().to_string();
    let (a, spake) = a.receive_hello(&hello).unwrap();
    let (b, confirm) = b.receive_spake(&spake).unwrap();
    let (_, reply1) = a.receive_confirm(&confirm).unwrap().approve(PAYLOAD).unwrap();
    assert!(b.receive_reply(&reply1).is_ok());

    // Session two: a registry holding the code (it never does, but grant it)
    // still cannot make the first reply open, since each side's SPAKE2
    // scalar is fresh.
    let (b2, _hello2) = PairB::start(binding(&s), PairingCode::parse(&shown).unwrap(), s.b.clone()).unwrap();
    let (b2, _) = b2.receive_spake(&spake).unwrap();
    assert!(b2.receive_reply(&reply1).is_err());
}

#[test]
fn d2_a_reply_from_another_session_is_refused() {
    let s = sides();
    let (run1, _, _) = full_run(&s);
    let (_, b2, _) = to_confirm(&s);
    assert!(b2.receive_reply(&run1.reply).is_err());
}

#[test]
fn d2_a_confirmation_or_ack_from_another_session_is_refused() {
    let s = sides();
    let (run1, _, _) = full_run(&s);
    let (a2, _, _) = to_confirm(&s);
    assert!(a2.receive_confirm(&run1.confirm).is_err());
    let (a3, _, _) = to_ack(&s);
    assert!(a3.receive_ack(&run1.ack).is_err());
    // A replayed hello against a fresh A with another code fails at the
    // confirmation, whatever B answers.
    let a4 = PairA::new(PeerKind::SamePersonDevice, USER, s.a_device).unwrap();
    let (a4, _) = a4.receive_hello(&run1.hello).unwrap();
    assert!(a4.receive_confirm(&run1.confirm).is_err());
}

// ------------------------------------------------ SPAKE2 interoperability

/// The RNG the vectors' scalars come out of: curve25519-dalek's
/// `Scalar::random` reduces 64 bytes wide, so the scalar's own 32
/// little-endian bytes and 32 zeros give back exactly that scalar.
struct Fixed([u8; 64], usize);

impl spake2::rand_core::TryRng for Fixed {
    type Error = std::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        unreachable!()
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        unreachable!()
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        dst.copy_from_slice(&self.0[self.1..self.1 + dst.len()]);
        self.1 += dst.len();
        Ok(())
    }
}

impl spake2::rand_core::TryCryptoRng for Fixed {}

fn fixed(le: &str) -> Fixed {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&krowk_client::e2e::unhex(le).unwrap());
    Fixed(b, 0)
}

/// python-spake2's asymmetric vector (password "password", idA "idA",
/// idB "idB"), which magic-wormhole's own implementation produces and the
/// spake2 crate reproduces in its private test: here through the public
/// API krowk calls, so a change to the crate that broke interop fails here.
/// The scalars are the vector's decimal ones, as 32 little-endian bytes.
#[test]
fn d2_spake2_matches_the_magic_wormhole_vectors() {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    let pw = Password::new(b"password");
    let (id_a, id_b) = (Identity::new(b"idA"), Identity::new(b"idB"));
    let (sa, msg_a) = Spake2::<Ed25519Group>::start_a_with_rng(
        &pw,
        &id_a,
        &id_b,
        fixed("25184061a70b1142f1a9f043a52cf7033dc308b5a0a32e42b003ecd59c2ac605"),
    );
    let (sb, msg_b) = Spake2::<Ed25519Group>::start_b_with_rng(
        &pw,
        &id_a,
        &id_b,
        fixed("9fb5e845084e0cbe27ac8b4d3af139b33f3f8a047d4234e2d45d33c5cd367b0f"),
    );
    let hex = krowk_client::e2e::hex;
    assert_eq!(hex(&msg_a), "416fc960df73c9cf8ed7198b0c9534e2e96a5984bfc5edc023fd24dacf371f2af9");
    assert_eq!(hex(&msg_b), "42354e97b88406922b1df4bea1d7870f17aed3dba7c720b313edae315b00959309");
    let ka = sa.finish(&msg_b).unwrap();
    let kb = sb.finish(&msg_a).unwrap();
    assert_eq!(ka, kb);
    assert_eq!(hex(&ka), "712295de7219c675ddd31942184aa26e0a957cf216bc230d165b215047b520c1");
}

// ------------------------------------------------------------- hardening

/// A user id past `MAX_USER_ID` is refused with an error on both sides,
/// never a panic, whatever a registry hands B.
#[test]
fn d2_an_overlong_user_id_is_an_error_not_a_panic() {
    let s = sides();
    for n in [65, 65_504, 65_535, 70_000] {
        let long = "u".repeat(n);
        assert!(PairA::new(PeerKind::SamePersonDevice, long.clone(), s.a_device).is_err(), "{n}");
        assert!(PairB::start(Binding { user_id: long, ..binding(&s) }, PairingCode::generate(), s.b.clone()).is_err(), "{n}");
    }
    assert!(PairA::new(PeerKind::SamePersonDevice, "", s.a_device).is_err());
    assert!(PairA::new(PeerKind::SamePersonDevice, "u".repeat(64), s.a_device).is_ok());
}

/// B's code is consumed by `start`: once moved in, the caller holds nothing
/// to start again with (`PairingCode` is not `Clone`; the doc test on it
/// shows `clone` does not compile). A second pairing needs a new code, and
/// a new parse of the same string is the caller's rule to refuse, as
/// `PairB::start`'s doc says.
#[test]
fn d2_bs_code_is_consumed_by_start() {
    type Start = fn(Binding, PairingCode, NewDevice) -> Result<(PairB, Vec<u8>), krowk_client::e2e::Error>;
    let _by_value: Start = PairB::start;
    let s = sides();
    let code = PairingCode::generate();
    let (_b, _hello) = PairB::start(binding(&s), code, s.b.clone()).unwrap();
    // `code` is moved: using it here would not compile.
}
