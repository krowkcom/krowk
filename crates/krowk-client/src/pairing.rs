//! Pairing a new device by a short code, against a registry that is
//! assumed hostile (engineering/devices.md in Canon, → Adding a device).
//!
//! The paired device, **A**, draws an eight-character code and shows it.
//! The new device, **B**, has the person type it. Under the code is SPAKE2
//! (magic-wormhole's construction on the Ed25519 group, asymmetric mode),
//! so the registry that carries the messages learns nothing it can test a
//! guess against offline. Playing man in the middle, it gets one online
//! guess against each side — one posing as B to A, one posing as A to B —
//! so 2 in 30⁸, about 2⁻³⁸, and a wrong guess ends that side's pairing.
//! That bound holds only if B never starts twice with one code (see
//! `PairB::start`). The SPAKE2 identities bind the protocol label, the
//! kind of peer, the person's user id and both devices' ids, so a
//! transcript cannot be moved to another person or another pair of
//! devices.
//!
//! SPAKE2 yields a key with no confirmation, so the flow adds it both ways
//! before anything is trusted:
//!
//! 1. B → A, `hello`: B's device id and its SPAKE2 message.
//! 2. A → B: A's SPAKE2 message.
//! 3. B → A, `confirm`: B's X25519 and Ed25519 keys, its name and OS, and
//!    a MAC over the whole transcript. A checks it, and that the device id
//!    in the hello is the one B's X25519 key hashes to, before the caller
//!    sees the name and OS it asks the person about.
//! 4. A → B, `reply`: sealed under the shared key, the caller's opaque
//!    payload (the signed chain, the user key wrapped to B, A's signing
//!    key) and A's own MAC. B opens it and checks A's MAC before it hands
//!    the payload out.
//! 5. B → A, `ack`: a MAC over the transcript and the reply. Only once A
//!    has checked it does the caller get the `Paired` it posts with, so a
//!    pairing that dies after the prompt leaves nothing on the chain.
//!
//! Every state is consumed by the step that leaves it, and a failed step
//! returns nothing to go on with: there is no retry against the same code.
//! Failures read alike, "the pairing failed", and never say which check it
//! was, which is all an attacker should learn.
//!
//! **What is not wiped.** The code, the derived keys, the payload and the
//! states' own secrets are zeroized on drop. What the `spake2` crate holds
//! is not: its `Password` copy of the code and its two scalars have no
//! `Zeroize`, and neither do the HKDF and HMAC states, so they are left in
//! freed memory. A memory disclosure during or just after a pairing could
//! show them; the code is single-use and dies with the pairing, so that is
//! accepted rather than worked around.
//!
//! **What the registry sees.** The hello and the confirmation go in the
//! clear: B's device id, public keys, name and OS, all of which the
//! registry records once the device is added anyway.

use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::Mac as _;
use sha2::{Digest as _, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::e2e::{DeviceId, DevicePublic, Error, SigningPublic, random};

/// The protocol's label, the first thing in both SPAKE2 identities and the
/// transcript.
pub const LABEL: &[u8] = b"krowk/pair/v1";
/// HKDF-SHA256's info for the key all three MACs are made under.
const CONFIRM_KEY: &[u8] = b"krowk/pair-confirm/v1";
/// HKDF-SHA256's info for the key the reply is sealed under: a key apart
/// from the MACs', so no MAC is ever made under a cipher's key.
const SEAL_KEY: &[u8] = b"krowk/pair-seal/v1";
/// Each MAC names what it is, so an ack can never stand in for a
/// confirmation, or A's reply MAC for either.
const MAC_B_CONFIRM: &[u8] = b"krowk/pair/v1 b-confirm";
const MAC_A_REPLY: &[u8] = b"krowk/pair/v1 a-reply";
const MAC_B_ACK: &[u8] = b"krowk/pair/v1 b-ack";
/// The reply's associated data starts with this.
const REPLY_AD: &[u8] = b"krowk/pair/v1 reply";

/// Every message's first byte, then its type.
const V1: u8 = 1;
const HELLO: u8 = 1;
const SPAKE_A: u8 = 2;
const CONFIRM: u8 = 3;
const REPLY: u8 = 4;
const ACK: u8 = 5;

/// A SPAKE2 message on the Ed25519 group: a side byte and a point.
const SPAKE_MSG: usize = 33;
const MAC: usize = 32;
const NONCE: usize = 24;
const TAG: usize = 16;

/// A device name and an OS reach a terminal prompt, so both are bounded
/// and hold no control or direction-changing characters. The bounds are the
/// device list's (`device_chain`: a name of 1–64 bytes, an OS of at most
/// 32), since what B confirms here is what A's `add` entry names: a longer
/// one would pass the pairing and be refused by every verifier after it.
pub const MAX_NAME: usize = 64;
pub const MAX_OS: usize = 32;

fn failed() -> Error {
    Error("the pairing failed — run `krowk devices add` again for a new code".into())
}

// ---------------------------------------------------------------- the code

/// Crockford's base32 alphabet less `0` and `1`: 30 symbols, none of
/// `0 O 1 I L U`, so nothing on the screen reads as something else.
pub const ALPHABET: &[u8; 30] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Eight symbols: 30⁸, about 39 bits, enough for one online guess a side.
pub const CODE_LEN: usize = 8;

/// The code the paired device shows and the new device's person types.
/// Held in canonical form (upper case, no separators), which is also the
/// SPAKE2 password. Wiped when dropped; `Debug` never shows it.
///
/// Neither `Clone` nor `PartialEq`: B's `PairB::start` takes it by value,
/// so one code starts one pairing, and nothing compares a typed code with
/// a shown one (SPAKE2 is the only comparison).
///
/// ```compile_fail,E0599
/// let code = krowk_client::pairing::PairingCode::generate();
/// let _again = code.clone();
/// ```
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PairingCode([u8; CODE_LEN]);

impl PairingCode {
    /// A new code from the operating system's random source. Rejection
    /// sampling, not `byte % 30`, so every symbol is equally likely.
    pub fn generate() -> PairingCode {
        let mut code = [0u8; CODE_LEN];
        let mut filled = 0;
        while filled < CODE_LEN {
            let mut pool: Zeroizing<[u8; 16]> = Zeroizing::new(random());
            for b in pool.iter_mut() {
                // 240 is the largest multiple of 30 a byte holds.
                if *b < 240 && filled < CODE_LEN {
                    code[filled] = ALPHABET[usize::from(*b % 30)];
                    filled += 1;
                }
                *b = 0;
            }
        }
        PairingCode(code)
    }

    /// A code as typed: case, spaces and dashes are ignored, as is
    /// whitespace around it (a pasted newline), and anything else — a symbol outside the alphabet, the wrong length — is refused.
    pub fn parse(typed: &str) -> Option<PairingCode> {
        let mut code = [0u8; CODE_LEN];
        let mut n = 0;
        for c in typed.trim().bytes() {
            if c == b' ' || c == b'-' {
                continue;
            }
            let c = c.to_ascii_uppercase();
            if n == CODE_LEN || !ALPHABET.contains(&c) {
                code.zeroize();
                return None;
            }
            code[n] = c;
            n += 1;
        }
        if n != CODE_LEN {
            code.zeroize();
            return None;
        }
        Some(PairingCode(code))
    }

    /// The canonical form, the SPAKE2 password.
    pub fn as_bytes(&self) -> &[u8; CODE_LEN] {
        &self.0
    }
}

/// Shown as `XXXX-XXXX`.
impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = std::str::from_utf8(&self.0).map_err(|_| std::fmt::Error)?;
        write!(f, "{}-{}", &s[..4], &s[4..])
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(…)")
    }
}

// --------------------------------------------------------- the identities

/// Who the two peers are to each other. Both sides must agree, since it is
/// in the SPAKE2 identities: a transcript for one kind fails as another.
///
/// The kind byte comes right after the label, before any other field, so
/// each kind defines its own fields under `krowk/pair/v1` without two
/// kinds ever encoding alike. Kind 2 is reserved for ticket 32's workspace
/// invite, which binds the inviter's user id, the invitee's user id and the
/// workspace id; it is not defined until then, since one user id cannot
/// bind two people and a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKind {
    /// Another device of the same person (`krowk devices add`): kind 1.
    SamePersonDevice,
}

impl PeerKind {
    fn byte(self) -> u8 {
        match self {
            PeerKind::SamePersonDevice => 1,
        }
    }
}

/// A user id is a UUID; anything longer is refused, never truncated.
pub const MAX_USER_ID: usize = 64;

/// What both sides bind: the peer kind, the person's user id, and the ids
/// of the paired device (A) and the new one (B). A knows all but B's id
/// when it shows the code, and learns that from B's hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub kind: PeerKind,
    pub user_id: String,
    pub a_device: DeviceId,
    pub b_device: DeviceId,
}

impl Binding {
    /// `LABEL ‖ kind ‖ len ‖ user id ‖ device id`, one per side. The user id
    /// is length-prefixed so no two bindings encode alike.
    fn identity(&self, device: &DeviceId) -> Result<Vec<u8>, Error> {
        let mut v = Vec::with_capacity(LABEL.len() + 3 + self.user_id.len() + 16);
        v.extend_from_slice(LABEL);
        v.push(self.kind.byte());
        if self.user_id.is_empty() {
            return Err(failed());
        }
        put_str(&mut v, &self.user_id, MAX_USER_ID)?;
        v.extend_from_slice(&device.0);
        Ok(v)
    }

    fn identities(&self) -> Result<(Identity, Identity, Vec<u8>), Error> {
        let a = self.identity(&self.a_device)?;
        let b = self.identity(&self.b_device)?;
        let mut both = Vec::with_capacity(a.len() + b.len() + 4);
        put_bytes(&mut both, &a)?;
        put_bytes(&mut both, &b)?;
        Ok((Identity::new(&a), Identity::new(&b), both))
    }
}

/// The new device as it describes itself, and as A, once B's MAC checks,
/// shows it to the person and keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDevice {
    pub device: DevicePublic,
    pub signing: SigningPublic,
    pub name: String,
    pub os: String,
}

impl NewDevice {
    pub fn id(&self) -> DeviceId {
        self.device.id()
    }
}

/// A name or an OS a terminal can print as it stands: not empty, bounded,
/// and none of the code points `device_chain::REFUSED_IN_NAMES` lists —
/// the control characters and Unicode's format and separator characters
/// (the direction marks and overrides, zero widths, the soft hyphen,
/// tags), which could make the prompt read as something it isn't. A
/// literal list, the chain's own, so the name pairing shows is one the
/// chain takes. Refused, not cleaned. Confusable letters (a Cyrillic `а`)
/// still pass: only the code's holder reaches the prompt.
fn printable(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(crate::device_chain::refused_in_name)
}

// ------------------------------------------------------------- the wire

fn put_bytes(v: &mut Vec<u8>, b: &[u8]) -> Result<(), Error> {
    let n = u16::try_from(b.len()).map_err(|_| failed())?;
    v.extend_from_slice(&n.to_be_bytes());
    v.extend_from_slice(b);
    Ok(())
}

fn put_str(v: &mut Vec<u8>, s: &str, max: usize) -> Result<(), Error> {
    if s.len() > max.min(usize::from(u16::MAX)) {
        return Err(failed());
    }
    put_bytes(v, s.as_bytes())
}

/// Reads a message front to back and refuses anything left over.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn header(b: &'a [u8], kind: u8) -> Result<Reader<'a>, Error> {
        match b {
            [V1, k, rest @ ..] if *k == kind => Ok(Reader(rest)),
            _ => Err(failed()),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(failed());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    fn string(&mut self, max: usize) -> Result<String, Error> {
        let n = u16::from_be_bytes(self.array()?);
        let s = std::str::from_utf8(self.take(usize::from(n))?).map_err(|_| failed())?;
        if !printable(s, max) {
            return Err(failed());
        }
        Ok(s.to_owned())
    }

    fn rest(self) -> &'a [u8] {
        self.0
    }

    fn end(self) -> Result<(), Error> {
        if self.0.is_empty() { Ok(()) } else { Err(failed()) }
    }
}

// -------------------------------------------------------- the key schedule

/// The two keys derived from SPAKE2's shared key, and the hash of the
/// transcript every MAC and the reply are bound to. Wiped when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
struct Keys {
    confirm: [u8; 32],
    seal: [u8; 32],
    transcript: [u8; 32],
}

impl Keys {
    fn derive(shared: &[u8], transcript: [u8; 32]) -> Keys {
        let hk = hkdf::Hkdf::<Sha256>::new(None, shared);
        let mut confirm = [0u8; 32];
        let mut seal = [0u8; 32];
        hk.expand(CONFIRM_KEY, &mut confirm).expect("32 bytes is a valid HKDF-SHA256 length");
        hk.expand(SEAL_KEY, &mut seal).expect("32 bytes is a valid HKDF-SHA256 length");
        Keys { confirm, seal, transcript }
    }

    fn mac(&self, what: &[u8], extra: &[u8]) -> [u8; MAC] {
        let mut m = hmac::Hmac::<Sha256>::new_from_slice(&self.confirm).expect("HMAC takes any key length");
        m.update(what);
        m.update(&self.transcript);
        m.update(extra);
        m.finalize().into_bytes().into()
    }

    /// In constant time, through `hmac`'s own comparison.
    fn verify(&self, what: &[u8], extra: &[u8], tag: &[u8]) -> Result<(), Error> {
        let mut m = hmac::Hmac::<Sha256>::new_from_slice(&self.confirm).expect("HMAC takes any key length");
        m.update(what);
        m.update(&self.transcript);
        m.update(extra);
        m.verify_slice(tag).map_err(|_| failed())
    }

    fn reply_ad(&self) -> Vec<u8> {
        [REPLY_AD, &[V1, REPLY], &self.transcript[..]].concat()
    }
}

/// SHA-256 over everything both sides said and bound: the identities, both
/// SPAKE2 messages, and B's keys, name and OS.
fn transcript(identities: &[u8], msg_a: &[u8], msg_b: &[u8], me: &NewDevice) -> Result<[u8; 32], Error> {
    let mut v = Vec::new();
    v.extend_from_slice(LABEL);
    v.extend_from_slice(identities);
    v.extend_from_slice(msg_a);
    v.extend_from_slice(msg_b);
    v.extend_from_slice(&me.device.0);
    v.extend_from_slice(&me.signing.0);
    put_str(&mut v, &me.name, MAX_NAME)?;
    put_str(&mut v, &me.os, MAX_OS)?;
    Ok(Sha256::digest(&v).into())
}

/// The operating system's random source, as the RNG SPAKE2 draws its
/// scalar from (its own `getrandom` feature stays off: see Cargo.toml).
struct OsRng;

impl spake2::rand_core::TryRng for OsRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(u32::from_le_bytes(random()))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(u64::from_le_bytes(random()))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        getrandom::fill(dst).expect("the operating system's random number generator failed");
        Ok(())
    }
}

impl spake2::rand_core::TryCryptoRng for OsRng {}

fn spake_key(state: Spake2<Ed25519Group>, theirs: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    check_point(theirs)?;
    state.finish(theirs).map(Zeroizing::new).map_err(|_| failed())
}

/// The peer's SPAKE2 point, after its side byte: on the curve, spelled the
/// one canonical way, and not one of the eight points of small order (or a
/// sum with one). Belt and braces: SPAKE2's blinding already keeps a
/// small-order point from revealing the code, but a peer sending one is
/// not following the protocol, and it is refused rather than reasoned
/// about (review n1 of #198).
fn check_point(msg: &[u8]) -> Result<(), Error> {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    let bytes: [u8; 32] = msg.get(1..).and_then(|b| b.try_into().ok()).ok_or_else(failed)?;
    let point = CompressedEdwardsY(bytes).decompress().ok_or_else(failed)?;
    if point.compress().0 != bytes || !point.is_torsion_free() || point.is_small_order() {
        return Err(failed());
    }
    Ok(())
}

// ------------------------------------------------------- A, the paired device

/// The paired device, showing its code and waiting for the new device's
/// hello.
pub struct PairA {
    code: PairingCode,
    kind: PeerKind,
    user_id: String,
    a_device: DeviceId,
}

impl PairA {
    /// A new pairing, with a new code from the OS random source. Refused
    /// for a user id that is empty or longer than `MAX_USER_ID`.
    pub fn new(kind: PeerKind, user_id: impl Into<String>, a_device: DeviceId) -> Result<PairA, Error> {
        let user_id = user_id.into();
        if user_id.is_empty() || user_id.len() > MAX_USER_ID {
            return Err(Error(format!("a user id is 1 to {MAX_USER_ID} bytes")));
        }
        Ok(PairA { code: PairingCode::generate(), kind, user_id, a_device })
    }

    /// The code to show, `XXXX-XXXX` when displayed.
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    /// Takes B's hello, and answers with A's SPAKE2 message.
    pub fn receive_hello(self, hello: &[u8]) -> Result<(AwaitConfirm, Vec<u8>), Error> {
        let mut r = Reader::header(hello, HELLO)?;
        let b_device = DeviceId(r.array()?);
        let msg_b = r.array::<SPAKE_MSG>()?;
        r.end()?;
        let binding = Binding { kind: self.kind, user_id: self.user_id, a_device: self.a_device, b_device };
        let (id_a, id_b, identities) = binding.identities()?;
        let (state, msg_a) =
            Spake2::<Ed25519Group>::start_a_with_rng(&Password::new(self.code.as_bytes()), &id_a, &id_b, OsRng);
        let shared = spake_key(state, &msg_b)?;
        let out = [&[V1, SPAKE_A][..], &msg_a].concat();
        Ok((AwaitConfirm { binding, identities, msg_a, msg_b: msg_b.to_vec(), shared }, out))
    }
}

/// A, waiting for B's confirmation.
pub struct AwaitConfirm {
    binding: Binding,
    identities: Vec<u8>,
    msg_a: Vec<u8>,
    msg_b: Vec<u8>,
    shared: Zeroizing<Vec<u8>>,
}

impl AwaitConfirm {
    /// Checks B's MAC over the transcript, and that its X25519 key is the
    /// device its hello named. A wrong code fails here, and ends the
    /// pairing.
    pub fn receive_confirm(self, confirm: &[u8]) -> Result<Confirmed, Error> {
        let mut r = Reader::header(confirm, CONFIRM)?;
        let device = DevicePublic(r.array()?);
        let signing = SigningPublic(r.array()?);
        let name = r.string(MAX_NAME)?;
        let os = r.string(MAX_OS)?;
        let tag = r.array::<MAC>()?;
        r.end()?;
        let new = NewDevice { device, signing, name, os };
        let keys = Keys::derive(&self.shared, transcript(&self.identities, &self.msg_a, &self.msg_b, &new)?);
        keys.verify(MAC_B_CONFIRM, &[], &tag)?;
        if new.id() != self.binding.b_device {
            return Err(failed());
        }
        Ok(Confirmed { binding: self.binding, new, keys })
    }
}

/// A, with B confirmed: what the person is asked about is `device()`, and
/// only what B's MAC covered.
pub struct Confirmed {
    binding: Binding,
    new: NewDevice,
    keys: Keys,
}

impl Confirmed {
    pub fn device(&self) -> &NewDevice {
        &self.new
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// The person said yes: seals `payload` (opaque here — the signed chain
    /// with the new `add` entry, the user key wrapped to B, A's signing key)
    /// with A's MAC, for B. Declining is dropping this.
    pub fn approve(self, payload: &[u8]) -> Result<(AwaitAck, Vec<u8>), Error> {
        let tag = self.keys.mac(MAC_A_REPLY, &Sha256::digest(payload));
        let plain = Zeroizing::new([payload, &tag[..]].concat());
        let nonce: [u8; NONCE] = random();
        let aead = XChaCha20Poly1305::new((&self.keys.seal).into());
        let sealed = aead
            .encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: &self.keys.reply_ad() })
            .map_err(|_| failed())?;
        let reply = [&[V1, REPLY][..], &nonce, &sealed].concat();
        let reply_hash: [u8; 32] = Sha256::digest(&reply).into();
        Ok((AwaitAck { binding: self.binding, new: self.new, keys: self.keys, reply_hash }, reply))
    }
}

/// A, waiting for B to acknowledge the reply.
pub struct AwaitAck {
    binding: Binding,
    new: NewDevice,
    keys: Keys,
    reply_hash: [u8; 32],
}

impl AwaitAck {
    /// Checks B's ack. Only the `Paired` this returns lets the caller post
    /// the `add` entry and the wrap.
    pub fn receive_ack(self, ack: &[u8]) -> Result<Paired, Error> {
        let mut r = Reader::header(ack, ACK)?;
        let tag = r.array::<MAC>()?;
        r.end()?;
        self.keys.verify(MAC_B_ACK, &self.reply_hash, &tag)?;
        Ok(Paired { binding: self.binding, device: self.new })
    }
}

/// Both sides confirmed: the one value A's caller posts to the registry
/// with. Its fields are private, so it comes only from a verified ack.
#[derive(Debug)]
pub struct Paired {
    binding: Binding,
    device: NewDevice,
}

impl Paired {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// The device to add: the keys, name and OS B's MAC covered.
    pub fn device(&self) -> &NewDevice {
        &self.device
    }
}

// ------------------------------------------------------ B, the new device

/// The new device, having sent its hello and waiting for A's SPAKE2
/// message.
pub struct PairB {
    binding: Binding,
    me: NewDevice,
    identities: Vec<u8>,
    msg_b: Vec<u8>,
    state: Spake2<Ed25519Group>,
}

impl PairB {
    /// Starts with the code the person typed and the binding: A's device id
    /// from the registry's routing, B's own from its key. A lying registry
    /// makes the keys differ, and the pairing fails at A.
    ///
    /// **The code is consumed.** Any failure on B's side — a step that
    /// fails, a timeout, a dropped connection, a transport error before or
    /// after the confirmation — throws the code away, and the person asks A
    /// for a new one. Never call `start` again with the same code, and never
    /// restart on the caller's own: every start gives a registry posing as
    /// A one more guess, since B's confirmation MAC lets it test one code
    /// offline. The caller must not keep the typed string to parse again.
    pub fn start(binding: Binding, code: PairingCode, me: NewDevice) -> Result<(PairB, Vec<u8>), Error> {
        if binding.b_device != me.id() || !printable(&me.name, MAX_NAME) || !printable(&me.os, MAX_OS) {
            return Err(Error("the new device's name or OS cannot be sent: use printable text, 64 and 32 bytes at most".into()));
        }
        let (id_a, id_b, identities) = binding.identities()?;
        let (state, msg_b) = Spake2::<Ed25519Group>::start_b_with_rng(&Password::new(code.as_bytes()), &id_a, &id_b, OsRng);
        let hello = [&[V1, HELLO][..], &binding.b_device.0, &msg_b].concat();
        Ok((PairB { binding, me, identities, msg_b, state }, hello))
    }

    /// Takes A's SPAKE2 message, and answers with B's confirmation.
    pub fn receive_spake(self, msg: &[u8]) -> Result<(AwaitReply, Vec<u8>), Error> {
        let mut r = Reader::header(msg, SPAKE_A)?;
        let msg_a = r.array::<SPAKE_MSG>()?;
        r.end()?;
        let shared = spake_key(self.state, &msg_a)?;
        let keys = Keys::derive(&shared, transcript(&self.identities, &msg_a, &self.msg_b, &self.me)?);
        let mut out = vec![V1, CONFIRM];
        out.extend_from_slice(&self.me.device.0);
        out.extend_from_slice(&self.me.signing.0);
        put_str(&mut out, &self.me.name, MAX_NAME)?;
        put_str(&mut out, &self.me.os, MAX_OS)?;
        out.extend_from_slice(&keys.mac(MAC_B_CONFIRM, &[]));
        Ok((AwaitReply { binding: self.binding, keys }, out))
    }
}

/// B, waiting for A's sealed reply.
pub struct AwaitReply {
    binding: Binding,
    keys: Keys,
}

impl AwaitReply {
    /// Opens A's reply and checks A's MAC. The payload is not yet to be
    /// kept: the caller checks the chain adds exactly B's keys, and only
    /// then acknowledges.
    pub fn receive_reply(self, reply: &[u8]) -> Result<Received, Error> {
        let mut r = Reader::header(reply, REPLY)?;
        let nonce = r.array::<NONCE>()?;
        let sealed = r.rest();
        if sealed.len() < MAC + TAG {
            return Err(failed());
        }
        let aead = XChaCha20Poly1305::new((&self.keys.seal).into());
        let plain = Zeroizing::new(
            aead.decrypt(&XNonce::from(nonce), Payload { msg: sealed, aad: &self.keys.reply_ad() })
                .map_err(|_| failed())?,
        );
        let (payload, tag) = plain.split_at(plain.len() - MAC);
        self.keys.verify(MAC_A_REPLY, &Sha256::digest(payload), tag)?;
        let reply_hash: [u8; 32] = Sha256::digest(reply).into();
        Ok(Received { binding: self.binding, payload: Zeroizing::new(payload.to_vec()), keys: self.keys, reply_hash })
    }
}

/// B, holding A's authenticated payload and not yet having acknowledged it.
/// Refusing it is dropping this.
pub struct Received {
    binding: Binding,
    payload: Zeroizing<Vec<u8>>,
    keys: Keys,
    reply_hash: [u8; 32],
}

impl Received {
    /// What A sealed: the chain, the wrap and A's signing key, for the
    /// caller to check before `acknowledge`. This module does not look
    /// inside, so those checks are the caller's: that the signing key and
    /// the chain entry's signer are `binding().a_device`'s, and that the
    /// new entry adds exactly this device's X25519 and Ed25519 keys.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// The payload checked out: the ack for A, and the payload to keep.
    pub fn acknowledge(self) -> (Zeroizing<Vec<u8>>, Vec<u8>) {
        let ack = [&[V1, ACK][..], &self.keys.mac(MAC_B_ACK, &self.reply_hash)].concat();
        (self.payload, ack)
    }
}
