//! The end-to-end constructions, and the only place krowk calls a
//! cryptographic primitive (R-E2E-2). Three layers, each key wrapping the
//! next (engineering/crypto.md in Canon has the diagram):
//!
//! - A **device key**: an X25519 keypair per machine and krowk home. The
//!   private half never leaves the machine.
//! - The **account key**: 32 random bytes, one per account, wrapped to each
//!   device's public key with HPKE (RFC 9180) — Base mode,
//!   DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305, and no
//!   other suite. The recovery phrase is this key, as words (`phrase`).
//! - A **session key**: 32 random bytes per session, wrapped under the
//!   account key with XChaCha20-Poly1305. Session content — batches and
//!   frames — is sealed under it by a `Sealer` and opened by an `Opener`: a
//!   fresh random 24-byte nonce per message, and as associated data the
//!   frame header with the session, the direction, the receiver's epoch and
//!   a per-direction counter the opener holds to strictly increasing.
//!
//! Every wrapped key is a versioned blob: its first byte is the format
//! version and its second the suite, both authenticated, so a later format
//! is refused by an older reader rather than misread. Every secret here is
//! wiped when dropped.
//!
//! Errors never say which check failed beyond "does not open": a wrong
//! key, a wrong device, a changed byte and a blob for another session all
//! read the same, which is all an attacker should learn.

use chacha20poly1305::aead::{Aead as _, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The HPKE suite, pinned: DHKEM(X25519, HKDF-SHA256).
pub type Kem = hpke::kem::X25519HkdfSha256;
/// HKDF-SHA256.
pub type Kdf = hpke::kdf::HkdfSha256;
/// ChaCha20-Poly1305 (HPKE's AEAD id 0x0003).
pub type HpkeAead = hpke::aead::ChaCha20Poly1305;

/// The WebSocket frame header's `enc` byte (crates/krowk-harness's
/// `daemon::ws`, offset 3) for a payload sealed with `seal`:
/// XChaCha20-Poly1305 under the session key, a random 24-byte nonce first,
/// the 28-byte header as associated data. 0 stays "none". Frames start
/// carrying it with the relay (tickets 17 and 19).
pub const ENC_XCHACHA20_POLY1305: u8 = 1;

/// Every blob's first byte.
pub const BLOB_V1: u8 = 1;
/// The account key wrapped to a device: HPKE, the suite above.
pub const SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305: u8 = 1;
/// A session key wrapped under the account key: XChaCha20-Poly1305.
pub const SUITE_XCHACHA20_POLY1305: u8 = 2;

const KEY: usize = 32;
const TAG: usize = 16;
const NONCE: usize = 24;
/// A wrapped account key: version, suite, HPKE's 32-byte `enc`, then the
/// sealed key and its tag.
pub const WRAPPED_ACCOUNT_KEY: usize = 2 + 32 + KEY + TAG;
/// A wrapped session key: version, suite, nonce, then the sealed key and
/// its tag.
pub const WRAPPED_SESSION_KEY: usize = 2 + NONCE + KEY + TAG;

/// Why something did not decrypt or decode. It names no secret and does not
/// say which check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(s: impl Into<String>) -> Error {
    Error(s.into())
}

/// A blob from a later krowk: the one refusal that says more than "does not
/// open", since only the version byte is looked at to say it. A version
/// below this krowk's is not a format that ever existed, and does not open.
fn newer(what: &str, v: u8) -> Error {
    err(format!("the wrapped {what} key is format {v}, newer than this krowk reads — upgrade krowk"))
}

/// Random bytes from the operating system. A machine that cannot give them
/// cannot make a key, and nothing krowk could do instead would be safe.
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the operating system's random number generator failed");
    b
}

/// A 16-byte id for a public thing, derived rather than stored, so it can
/// never disagree with what it names: SHA-256 over a label and the bytes.
fn id(label: &[u8], bytes: &[u8]) -> [u8; 16] {
    let h = Sha256::new().chain_update(label).chain_update(bytes).finalize();
    h[..16].try_into().expect("sixteen bytes")
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    // Hex digits only: `from_str_radix` alone would take a sign ("+f").
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// A device's keypair. The private half is wiped when this is dropped
/// (x25519-dalek's `StaticSecret`, under hpke's key type).
pub struct DeviceKey {
    secret: <Kem as hpke::Kem>::PrivateKey,
    public: <Kem as hpke::Kem>::PublicKey,
}

impl DeviceKey {
    /// A new keypair: RFC 9180's DeriveKeyPair over 32 random bytes.
    pub fn generate() -> DeviceKey {
        let (secret, public) = Kem::gen_keypair();
        DeviceKey { secret, public }
    }

    /// The keypair from its stored private half.
    pub fn from_secret(bytes: &[u8]) -> Result<DeviceKey, Error> {
        let secret = <Kem as hpke::Kem>::PrivateKey::from_bytes(bytes).map_err(|_| err("the device key is not an X25519 private key"))?;
        let public = Kem::sk_to_pk(&secret);
        Ok(DeviceKey { secret, public })
    }

    /// The private half, for the keystore to write. Wiped when dropped.
    pub fn secret_bytes(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.secret.to_bytes().into())
    }

    pub fn public(&self) -> DevicePublic {
        DevicePublic(self.public.to_bytes().into())
    }

    pub fn id(&self) -> DeviceId {
        self.public().id()
    }
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.id())
    }
}

/// A device's public key: what another device (and later the registry)
/// holds to wrap the account key to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePublic(pub [u8; 32]);

impl DevicePublic {
    pub fn id(&self) -> DeviceId {
        DeviceId(id(b"krowk/device-id/v1", &self.0))
    }
}

/// A device's id: a hash of its public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceId(pub [u8; 16]);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl DeviceId {
    /// An id as a person types it back: 32 hex characters, in any case, with
    /// the spaces or dashes it was shown grouped by ignored.
    pub fn parse(typed: &str) -> Option<DeviceId> {
        parse_id(typed).map(DeviceId)
    }

    /// The id grouped in fours, to be read off one screen and compared with
    /// another.
    pub fn grouped(&self) -> String {
        grouped(&self.to_string())
    }
}

fn parse_id(typed: &str) -> Option<[u8; 16]> {
    let hex: String = typed.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    unhex(&hex.to_ascii_lowercase()).and_then(|b| b.try_into().ok())
}

fn grouped(hex: &str) -> String {
    hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The account key's id: a hash of the key, so a phrase can be checked
/// against the key a device already holds, and a wrapped key names which
/// account key it is without saying anything about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyId(pub [u8; 16]);

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl KeyId {
    /// As `DeviceId::parse`: 32 hex characters, spaces and dashes ignored.
    pub fn parse(typed: &str) -> Option<KeyId> {
        parse_id(typed).map(KeyId)
    }

    pub fn grouped(&self) -> String {
        grouped(&self.to_string())
    }
}

/// The account key. Wiped when dropped; never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct AccountKey([u8; KEY]);

impl AccountKey {
    pub fn generate() -> AccountKey {
        AccountKey(random())
    }

    pub fn from_bytes(b: [u8; KEY]) -> AccountKey {
        AccountKey(b)
    }

    pub fn as_bytes(&self) -> &[u8; KEY] {
        &self.0
    }

    pub fn id(&self) -> KeyId {
        KeyId(id(b"krowk/account-key-id/v1", &self.0))
    }
}

impl PartialEq for AccountKey {
    /// In constant time: comparing a restored key with the one on disk
    /// should not say how many bytes matched.
    fn eq(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

impl std::fmt::Debug for AccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AccountKey({})", self.id())
    }
}

/// A session's content key. Wiped when dropped; never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SessionKey([u8; KEY]);

impl SessionKey {
    pub fn generate() -> SessionKey {
        SessionKey(random())
    }

    pub fn as_bytes(&self) -> &[u8; KEY] {
        &self.0
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(…)")
    }
}

/// HPKE's `info`: what the wrapped account key is for — this account key
/// (by id), this device — so a blob moved to another device, or passed off
/// as another account's, does not open.
fn account_info(key: KeyId, device: DeviceId) -> Vec<u8> {
    [&b"krowk/account-key/v1"[..], &key.0, &device.0].concat()
}

/// The account key wrapped to one device: `version | suite | enc | sealed
/// key + tag` (82 bytes). Version and suite are the associated data.
pub fn wrap_account_key(account: &AccountKey, to: &DevicePublic) -> Result<Vec<u8>, Error> {
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(&to.0).map_err(|_| err("the device's public key is not an X25519 key"))?;
    let head = [BLOB_V1, SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305];
    let info = account_info(account.id(), to.id());
    let (enc, sealed) = hpke::single_shot_seal::<HpkeAead, Kdf, Kem>(&OpModeS::Base, &pk, &info, &account.0, &head).map_err(|_| err("the account key could not be wrapped"))?;
    Ok([&head[..], &enc.to_bytes(), &sealed].concat())
}

/// The account key out of a blob `wrap_account_key` made for this device,
/// and for the account key `key` names.
pub fn unwrap_account_key(blob: &[u8], key: KeyId, device: &DeviceKey) -> Result<AccountKey, Error> {
    let refused = || err("the wrapped account key does not open with this device's key: it was changed, or was wrapped for another device or account");
    if blob.len() != WRAPPED_ACCOUNT_KEY || blob[0] != BLOB_V1 || blob[1] != SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305 {
        return Err(match blob.first() {
            Some(&v) if v > BLOB_V1 => newer("account", v),
            _ => refused(),
        });
    }
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&blob[2..34]).map_err(|_| refused())?;
    let info = account_info(key, device.id());
    let mut plain = hpke::single_shot_open::<HpkeAead, Kdf, Kem>(&OpModeR::Base, &device.secret, &enc, &info, &blob[34..], &blob[..2]).map_err(|_| refused())?;
    let out = <[u8; KEY]>::try_from(&plain[..]).map(AccountKey);
    plain.zeroize();
    let account = out.map_err(|_| refused())?;
    // The id is in `info`, so a key that opens is the key it names; checked
    // again all the same, since it costs a hash.
    if account.id() != key {
        return Err(refused());
    }
    Ok(account)
}

/// The format byte of a sealed session index.
pub const INDEX_V1: u8 = 1;

/// A session's index — its title and whatever else a listing shows — sealed
/// under its session key for the registry to hold: `version | suite | nonce
/// | ciphertext + tag`. The associated data is a label, the version, the
/// suite and the session's id, so an index moved to another session, or
/// presented as a wrapped key, does not open.
///
/// One message, replaced whenever the holder writes it, so there is no
/// counter chain to bind: a registry can hand back an older index than the
/// latest (a rollback), which shows a stale title and nothing more. The
/// chunks, which carry the log, bind their order (ticket 19).
pub fn seal_session_index(key: &SessionKey, session: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let head = [INDEX_V1, SUITE_XCHACHA20_POLY1305];
    let nonce: [u8; NONCE] = random();
    let aad = [&b"krowk/session-index/v1"[..], &head, session].concat();
    let sealed = cipher(&key.0).encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: &aad }).expect("an index is far below XChaCha's limit");
    [&head[..], &nonce, &sealed].concat()
}

pub fn open_session_index(blob: &[u8], session: &[u8; 16], key: &SessionKey) -> Result<Vec<u8>, Error> {
    let refused = || err("the session index does not open with this session's key: it was changed, or belongs to another session");
    if blob.len() < 2 + NONCE + TAG || blob[0] != INDEX_V1 || blob[1] != SUITE_XCHACHA20_POLY1305 {
        return Err(match blob.first() {
            Some(&v) if v > INDEX_V1 => err(format!("the session index is format {v}, newer than this krowk reads — upgrade krowk")),
            _ => refused(),
        });
    }
    let nonce: [u8; NONCE] = blob[2..2 + NONCE].try_into().expect("24 bytes");
    let aad = [&b"krowk/session-index/v1"[..], &blob[..2], session].concat();
    cipher(&key.0).decrypt(&XNonce::from(nonce), Payload { msg: &blob[2 + NONCE..], aad: &aad }).map_err(|_| refused())
}

fn cipher(key: &[u8; KEY]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&(*key).into())
}

/// The session key wrapped under the account key: `version | suite | nonce
/// | sealed key + tag` (74 bytes). The associated data is version, suite and
/// the session's 16-byte id, so a wrapped key moved to another session does
/// not open.
pub fn wrap_session_key(session_key: &SessionKey, session: &[u8; 16], account: &AccountKey) -> Vec<u8> {
    let head = [BLOB_V1, SUITE_XCHACHA20_POLY1305];
    let nonce: [u8; NONCE] = random();
    let aad = [&head[..], session].concat();
    let sealed = cipher(&account.0).encrypt(&XNonce::from(nonce), Payload { msg: &session_key.0, aad: &aad }).expect("sealing 32 bytes cannot fail");
    [&head[..], &nonce, &sealed].concat()
}

pub fn unwrap_session_key(blob: &[u8], session: &[u8; 16], account: &AccountKey) -> Result<SessionKey, Error> {
    let refused = || err("the wrapped session key does not open with this account key: it was changed, or belongs to another session or account");
    if blob.len() != WRAPPED_SESSION_KEY || blob[0] != BLOB_V1 || blob[1] != SUITE_XCHACHA20_POLY1305 {
        return Err(match blob.first() {
            Some(&v) if v > BLOB_V1 => newer("session", v),
            _ => refused(),
        });
    }
    let nonce: [u8; NONCE] = blob[2..2 + NONCE].try_into().expect("24 bytes");
    let aad = [&blob[..2], session].concat();
    let mut plain = cipher(&account.0).decrypt(&XNonce::from(nonce), Payload { msg: &blob[2 + NONCE..], aad: &aad }).map_err(|_| refused())?;
    let out = <[u8; KEY]>::try_from(&plain[..]).map(SessionKey);
    plain.zeroize();
    out.map_err(|_| refused())
}

/// Which way a sealed message travels. Each direction of a session is its
/// own counter chain, so a message cannot be reflected back at its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From the host holding the session's lease to a client: batches.
    HostToClient = 1,
    /// From a client to that host: frames (prompts, commands, approvals).
    ClientToHost = 2,
}

/// The frame header's length: `enc` is its byte 3.
pub const HEADER: usize = 28;
/// A sealed payload: counter (8, big-endian), nonce (24), ciphertext, tag.
pub const SEALED_OVERHEAD: usize = 8 + NONCE + TAG;

/// What the content AEAD binds besides the frame header: the header alone
/// repeats (seq 0 on a batch with no line, session and seq 0 on a client's
/// frame), and a header that repeats binds no position. This names the
/// session, the direction, the receiver's epoch for this chain and the
/// message's place in it, so the associated data of every message sealed
/// under one session key is unique.
fn frame_aad(header: &[u8; HEADER], session: &[u8; 16], direction: Direction, epoch: &[u8; 16], counter: u64) -> Vec<u8> {
    [&b"krowk/frame/v1"[..], header, session, &[direction as u8], epoch, &counter.to_be_bytes()].concat()
}

fn check_header(header: &[u8; HEADER]) -> Result<(), Error> {
    if header[3] != ENC_XCHACHA20_POLY1305 {
        return Err(err(format!("a sealed frame's header says enc {}, not {ENC_XCHACHA20_POLY1305}: set it before sealing", header[3])));
    }
    Ok(())
}

/// The sending end of one direction of one session's messages: seals each
/// under the session key with the next counter, 0 first. `epoch` is the
/// receiver's (`Opener::epoch`), sent to the sender in the clear when the
/// connection opens, so a message recorded on one connection opens on no
/// other — the receiver picked a fresh one.
pub struct Sealer {
    key: SessionKey,
    session: [u8; 16],
    direction: Direction,
    epoch: [u8; 16],
    next: u64,
}

impl Sealer {
    pub fn new(key: &SessionKey, session: [u8; 16], direction: Direction, epoch: [u8; 16]) -> Sealer {
        Sealer { key: key.clone(), session, direction, epoch, next: 0 }
    }

    /// `counter ‖ nonce ‖ ciphertext + tag`. The header's `enc` must be
    /// `ENC_XCHACHA20_POLY1305`. The nonce is 24 random bytes; the counter
    /// orders messages, never feeds the nonce.
    pub fn seal(&mut self, header: &[u8; HEADER], plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        check_header(header)?;
        let counter = self.next;
        self.next = self.next.checked_add(1).ok_or_else(|| err("a session's message counter ran out; start a new connection"))?;
        let nonce: [u8; NONCE] = random();
        let aad = frame_aad(header, &self.session, self.direction, &self.epoch, counter);
        let sealed = cipher(&self.key.0).encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: &aad }).map_err(|_| err("the payload is too large to seal"))?;
        Ok([&counter.to_be_bytes()[..], &nonce, &sealed].concat())
    }
}

/// The receiving end of one direction of one session's messages: opens
/// only what was sealed for this session, direction and epoch, and only a
/// counter strictly greater than the last one opened — a replay, a
/// reordering or a message from another chain is refused. A gap (a counter
/// that skips) opens: the transport may legitimately drop and catch a
/// client up from its cursor, and `last` says where it now is.
pub struct Opener {
    key: SessionKey,
    session: [u8; 16],
    direction: Direction,
    epoch: [u8; 16],
    last: Option<u64>,
}

impl Opener {
    /// A new chain with a fresh random epoch, which the peer's `Sealer`
    /// must be given.
    pub fn new(key: &SessionKey, session: [u8; 16], direction: Direction) -> Opener {
        Opener { key: key.clone(), session, direction, epoch: random(), last: None }
    }

    pub fn epoch(&self) -> [u8; 16] {
        self.epoch
    }

    /// The counter of the last message opened.
    pub fn last(&self) -> Option<u64> {
        self.last
    }

    pub fn open(&mut self, header: &[u8; HEADER], sealed: &[u8]) -> Result<Vec<u8>, Error> {
        check_header(header)?;
        let refused = || err("the payload does not open with this session's key: it was changed, or belongs to another frame, direction, connection or session");
        if sealed.len() < SEALED_OVERHEAD {
            return Err(refused());
        }
        let counter = u64::from_be_bytes(sealed[..8].try_into().expect("eight bytes"));
        if self.last.is_some_and(|last| counter <= last) {
            return Err(err(format!("message {counter} arrived after message {}: a replay or a reordering, refused", self.last.unwrap_or_default())));
        }
        let nonce: [u8; NONCE] = sealed[8..8 + NONCE].try_into().expect("24 bytes");
        let aad = frame_aad(header, &self.session, self.direction, &self.epoch, counter);
        let plain = cipher(&self.key.0).decrypt(&XNonce::from(nonce), Payload { msg: &sealed[8 + NONCE..], aad: &aad }).map_err(|_| refused())?;
        // Only an authentic message moves the chain on.
        self.last = Some(counter);
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: [u8; 16] = *b"0123456789abcdef";

    /// A 28-byte header as the WebSocket transport lays it out, `enc` set.
    fn header(seq: u64) -> [u8; HEADER] {
        [&[1u8, 1, 0, ENC_XCHACHA20_POLY1305][..], &SESSION, &seq.to_be_bytes()].concat().try_into().unwrap()
    }

    /// The header every control-only batch shares: seq 0.
    fn repeated() -> [u8; HEADER] {
        header(0)
    }

    fn pair(key: &SessionKey, d: Direction) -> (Sealer, Opener) {
        let o = Opener::new(key, SESSION, d);
        (Sealer::new(key, SESSION, d, o.epoch()), o)
    }

    fn flip(b: &[u8], i: usize) -> Vec<u8> {
        let mut v = b.to_vec();
        v[i] ^= 1;
        v
    }

    #[test]
    fn r_e2e_3_the_account_key_wraps_to_a_device_and_unwraps_there_only() {
        let device = DeviceKey::generate();
        let account = AccountKey::generate();
        let blob = wrap_account_key(&account, &device.public()).unwrap();
        assert_eq!(blob.len(), WRAPPED_ACCOUNT_KEY);
        assert_eq!(&blob[..2], &[BLOB_V1, SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305]);
        assert_eq!(unwrap_account_key(&blob, account.id(), &device).unwrap(), account);
        // Wrapped twice, it is two different blobs: a fresh HPKE ephemeral
        // key every time.
        assert_ne!(wrap_account_key(&account, &device.public()).unwrap(), blob);

        let other = DeviceKey::generate();
        assert!(unwrap_account_key(&blob, account.id(), &other).is_err(), "another device's key opens it");
        assert!(unwrap_account_key(&blob, AccountKey::generate().id(), &device).is_err(), "it opens under another account's id");
        // Stored and read back, the device key is the same key.
        let again = DeviceKey::from_secret(&device.secret_bytes()[..]).unwrap();
        assert_eq!(again.public(), device.public());
        assert_eq!(unwrap_account_key(&blob, account.id(), &again).unwrap(), account);
    }

    #[test]
    fn r_e2e_2_a_changed_byte_anywhere_in_a_wrapped_account_key_refuses_it() {
        let device = DeviceKey::generate();
        let account = AccountKey::generate();
        let blob = wrap_account_key(&account, &device.public()).unwrap();
        for i in 0..blob.len() {
            assert!(unwrap_account_key(&flip(&blob, i), account.id(), &device).is_err(), "byte {i} changed and it still opened");
        }
        assert!(unwrap_account_key(&blob[..blob.len() - 1], account.id(), &device).is_err());
        let later = [&[2u8][..], &blob[1..]].concat();
        assert!(unwrap_account_key(&later, account.id(), &device).unwrap_err().0.contains("format 2, newer"));
        let never = [&[0u8][..], &blob[1..]].concat();
        assert!(!unwrap_account_key(&never, account.id(), &device).unwrap_err().0.contains("upgrade"));
    }

    #[test]
    fn r_e2e_2_session_keys_wrap_under_the_account_key_and_refuse_tampering() {
        let account = AccountKey::generate();
        let key = SessionKey::generate();
        let blob = wrap_session_key(&key, &SESSION, &account);
        assert_eq!(blob.len(), WRAPPED_SESSION_KEY);
        assert_eq!(unwrap_session_key(&blob, &SESSION, &account).unwrap().as_bytes(), key.as_bytes());
        for i in 0..blob.len() {
            assert!(unwrap_session_key(&flip(&blob, i), &SESSION, &account).is_err(), "byte {i} changed and it still opened");
        }
        assert!(unwrap_session_key(&blob, b"another-session!", &account).is_err(), "it opened for another session");
        let later = [&[2u8][..], &blob[1..]].concat();
        assert!(unwrap_session_key(&later, &SESSION, &account).unwrap_err().0.contains("format 2, newer"));
        assert!(unwrap_session_key(&blob, &SESSION, &AccountKey::generate()).is_err(), "another account key opened it");
    }

    #[test]
    fn r_e2e_2_content_is_bound_to_its_frame_header() {
        let key = SessionKey::generate();
        let text = br#"{"type":"delta","text":"hello"}"#;
        let (mut tx, mut rx) = pair(&key, Direction::HostToClient);
        let sealed = tx.seal(&header(7), text).unwrap();
        assert_eq!(sealed.len(), SEALED_OVERHEAD + text.len());
        // Under another header it does not open (and a failed open does
        // not move the chain on).
        let mut other = Opener { epoch: rx.epoch(), ..Opener::new(&key, SESSION, Direction::HostToClient) };
        assert!(other.open(&header(8), &sealed).is_err(), "moved to another seq");
        let mut moved = header(7);
        moved[5] ^= 1;
        assert!(other.open(&moved, &sealed).is_err(), "moved to another session's header");
        for i in 0..sealed.len() {
            assert!(other.open(&header(7), &flip(&sealed, i)).is_err(), "byte {i} changed and it still opened");
        }
        assert_eq!(rx.open(&header(7), &sealed).unwrap(), text);
        assert!(Opener::new(&SessionKey::generate(), SESSION, Direction::HostToClient).open(&header(7), &sealed).is_err());
        assert!(rx.open(&header(7), &sealed[..SEALED_OVERHEAD - 1]).is_err());
    }

    /// The review's attack: headers repeat (every control-only batch, every
    /// client frame), so the header alone binds no position. The counter,
    /// direction and receiver's epoch do.
    #[test]
    fn r_e2e_2_a_replayed_reordered_or_reflected_message_is_refused() {
        let key = SessionKey::generate();
        let (mut tx, mut rx) = pair(&key, Direction::ClientToHost);
        let approve = tx.seal(&repeated(), b"approve").unwrap();
        let deny = tx.seal(&repeated(), b"deny").unwrap();
        // Reordered: the second first, then the first is refused.
        assert_eq!(rx.open(&repeated(), &deny).unwrap(), b"deny");
        assert!(rx.open(&repeated(), &approve).unwrap_err().0.contains("replay or a reordering"));
        // Replayed: the same message twice.
        let again = tx.seal(&repeated(), b"execute").unwrap();
        assert_eq!(rx.open(&repeated(), &again).unwrap(), b"execute");
        assert!(rx.open(&repeated(), &again).is_err());
        assert_eq!(rx.last(), Some(2));
        // A gap opens, and says where the chain is.
        tx.seal(&repeated(), b"dropped").unwrap();
        assert_eq!(rx.open(&repeated(), &tx.seal(&repeated(), b"after").unwrap()).unwrap(), b"after");
        assert_eq!(rx.last(), Some(4));

        // Reflected: a host-to-client message fed to the client-to-host
        // opener (same key, same session, same epoch, fresh counter).
        let mut back = Sealer::new(&key, SESSION, Direction::HostToClient, rx.epoch());
        back.next = 9;
        assert!(rx.open(&repeated(), &back.seal(&repeated(), b"x").unwrap()).is_err(), "the other direction opened");
        // Another connection: a recording of this one opens on no new opener.
        let mut next_conn = Opener::new(&key, SESSION, Direction::ClientToHost);
        assert!(next_conn.open(&repeated(), &approve).is_err(), "replayed across connections");
        // Another session under the same key.
        let mut elsewhere = Opener { epoch: rx.epoch(), ..Opener::new(&key, *b"another-session!", Direction::ClientToHost) };
        assert!(elsewhere.open(&repeated(), &approve).is_err());
    }

    #[test]
    fn r_e2e_2_a_header_without_the_enc_byte_is_refused() {
        let key = SessionKey::generate();
        let (mut tx, mut rx) = pair(&key, Direction::HostToClient);
        let mut plain = header(1);
        plain[3] = 0;
        assert!(tx.seal(&plain, b"x").unwrap_err().0.contains("enc 0"));
        let sealed = tx.seal(&header(1), b"x").unwrap();
        assert!(rx.open(&plain, &sealed).is_err());
    }

    #[test]
    fn r_e2e_2_the_enc_byte_is_the_one_the_frame_header_reserves() {
        // 0 is "none" in the header (daemon::ws::ENC_NONE); this is the
        // first value the transport was told to wait for.
        assert_eq!(ENC_XCHACHA20_POLY1305, 1);
        assert_eq!(header(0)[3], ENC_XCHACHA20_POLY1305);
    }

    #[test]
    fn nothing_secret_is_printed() {
        let account = AccountKey::generate();
        let shown = format!("{account:?} {:?} {:?}", SessionKey::generate(), DeviceKey::generate());
        assert!(!shown.contains(&hex(account.as_bytes())), "{shown}");
        assert!(shown.contains(&account.id().to_string()));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
        assert!(unhex("+f+f").is_none(), "a sign is not a hex digit");
    }
    /// R-E2E-1: a session's index — its title — reaches the registry sealed
    /// under the session key, opens only for that session, and a changed
    /// byte does not open at all.
    #[test]
    fn r_e2e_1_a_session_index_opens_only_for_its_session() {
        let key = SessionKey::generate();
        let sealed = seal_session_index(&key, &SESSION, b"fix the flaky test");
        assert!(!sealed.windows(5).any(|w| w == b"flaky"), "the title is not in the blob");
        assert_eq!(open_session_index(&sealed, &SESSION, &key).unwrap(), b"fix the flaky test");
        assert!(open_session_index(&sealed, b"fedcba9876543210", &key).is_err(), "moved to another session");
        assert!(open_session_index(&sealed, &SESSION, &SessionKey::generate()).is_err(), "another session's key");
        let mut changed = sealed.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(open_session_index(&changed, &SESSION, &key).is_err());
        let mut newer = sealed;
        newer[0] = INDEX_V1 + 1;
        assert!(open_session_index(&newer, &SESSION, &key).unwrap_err().0.contains("upgrade krowk"));
    }

    #[test]
    fn ids_read_back_as_they_were_shown() {
        let d = DeviceKey::generate().id();
        assert_eq!(d.grouped().len(), 32 + 7);
        assert_eq!(DeviceId::parse(&d.grouped()), Some(d));
        assert_eq!(DeviceId::parse(&d.grouped().to_uppercase().replace(' ', "-")), Some(d));
        assert_eq!(DeviceId::parse(&d.to_string()[..30]), None);
        let k = AccountKey::generate().id();
        assert_eq!(KeyId::parse(&k.grouped()), Some(k));
    }
}
