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
//!   chunks — is sealed under it, a fresh random 24-byte nonce per message,
//!   the message's frame header as associated data.
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
    if !s.len().is_multiple_of(2) {
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
            Some(&v) if v != BLOB_V1 => err(format!("the wrapped account key is format {v}, which this krowk does not read — upgrade krowk")),
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
        return Err(refused());
    }
    let nonce: [u8; NONCE] = blob[2..2 + NONCE].try_into().expect("24 bytes");
    let aad = [&blob[..2], session].concat();
    let mut plain = cipher(&account.0).decrypt(&XNonce::from(nonce), Payload { msg: &blob[2 + NONCE..], aad: &aad }).map_err(|_| refused())?;
    let out = <[u8; KEY]>::try_from(&plain[..]).map(SessionKey);
    plain.zeroize();
    out.map_err(|_| refused())
}

/// Session content sealed under its session key: a random 24-byte nonce,
/// then the ciphertext and tag. `header` is the associated data — the
/// message's 28-byte frame header, `enc` set to `ENC_XCHACHA20_POLY1305` —
/// so a sealed payload moved under another header (another session,
/// another seq, another kind) does not open. XChaCha's nonce is long
/// enough to draw at random for every message with no counter kept.
pub fn seal(key: &SessionKey, header: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let nonce: [u8; NONCE] = random();
    let sealed = cipher(&key.0).encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: header }).expect("sealing cannot fail below XChaCha20's 256 GiB message limit");
    [&nonce[..], &sealed].concat()
}

pub fn open(key: &SessionKey, header: &[u8], sealed: &[u8]) -> Result<Vec<u8>, Error> {
    let refused = || err("the payload does not open with this session's key: it was changed, or belongs to another frame or session");
    if sealed.len() < NONCE + TAG {
        return Err(refused());
    }
    let nonce: [u8; NONCE] = sealed[..NONCE].try_into().expect("24 bytes");
    cipher(&key.0).decrypt(&XNonce::from(nonce), Payload { msg: &sealed[NONCE..], aad: header }).map_err(|_| refused())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: [u8; 16] = *b"0123456789abcdef";

    /// A 28-byte header as the WebSocket transport lays it out, `enc` set.
    fn header(seq: u64) -> Vec<u8> {
        [&[1u8, 1, 0, ENC_XCHACHA20_POLY1305][..], &SESSION, &seq.to_be_bytes()].concat()
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
        assert!(unwrap_account_key(&later, account.id(), &device).unwrap_err().0.contains("format 2"));
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
        assert!(unwrap_session_key(&blob, &SESSION, &AccountKey::generate()).is_err(), "another account key opened it");
    }

    #[test]
    fn r_e2e_2_content_is_bound_to_its_frame_header() {
        let key = SessionKey::generate();
        let text = br#"{"type":"delta","text":"hello"}"#;
        let sealed = seal(&key, &header(7), text);
        assert_eq!(sealed.len(), NONCE + text.len() + TAG);
        assert_eq!(open(&key, &header(7), &sealed).unwrap(), text);
        // A nonce is drawn per message: the same text twice is two ciphertexts.
        assert_ne!(seal(&key, &header(7), text), sealed);
        assert!(open(&key, &header(8), &sealed).is_err(), "replayed under another seq");
        assert!(open(&key, &flip(&header(7), 5), &sealed).is_err(), "moved to another session");
        for i in 0..sealed.len() {
            assert!(open(&key, &header(7), &flip(&sealed, i)).is_err(), "byte {i} changed and it still opened");
        }
        assert!(open(&SessionKey::generate(), &header(7), &sealed).is_err());
        assert!(open(&key, &header(7), &sealed[..NONCE + TAG - 1]).is_err());
    }

    #[test]
    fn r_e2e_2_the_enc_byte_is_the_one_the_frame_header_reserves() {
        // 0 is "none" in the header (daemon::ws::ENC_NONE); this is the
        // first value the transport was told to wait for.
        assert_eq!(ENC_XCHACHA20_POLY1305, 1);
        assert_eq!(header(0).len(), 28);
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
    }
}
