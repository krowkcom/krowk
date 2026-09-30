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
//! - A **session key**: 32 random bytes per session, wrapped under its
//!   owner's user key (`user_key`), at the generation current when it was
//!   sealed, with XChaCha20-Poly1305. Session content — batches and
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
use crate::user_key::{UserKey, UserKeys};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The HPKE suite, pinned: DHKEM(X25519, HKDF-SHA256).
pub type Kem = hpke::kem::X25519HkdfSha256;
/// HKDF-SHA256.
pub type Kdf = hpke::kdf::HkdfSha256;
/// ChaCha20-Poly1305 (HPKE's AEAD id 0x0003).
pub type HpkeAead = hpke::aead::ChaCha20Poly1305;

/// The frame header's `enc` byte for a payload sealed with `seal`, declared
/// with the rest of the header in `protocol::frame`.
pub use crate::protocol::frame::ENC_XCHACHA20_POLY1305;

/// Every blob's first byte.
pub const BLOB_V1: u8 = 1;
/// The account key wrapped to a device: HPKE, the suite above.
pub const SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305: u8 = 1;
/// A key wrapped under another 32-byte key: XChaCha20-Poly1305.
pub const SUITE_XCHACHA20_POLY1305: u8 = 2;

const KEY: usize = 32;
const TAG: usize = 16;
const NONCE: usize = 24;
/// A wrapped account key: version, suite, HPKE's 32-byte `enc`, then the
/// sealed key and its tag.
pub const WRAPPED_ACCOUNT_KEY: usize = 2 + 32 + KEY + TAG;
/// A wrapped session key: version, suite, a nonce that begins with the user
/// key generation, then the sealed key and its tag (`wrap_session_key`).
pub const WRAPPED_SESSION_KEY: usize = 2 + NONCE + KEY + TAG;
/// A wrapped session key's version: 2 since it is sealed under the user
/// key, which a version-1 blob (under the account key) never was.
pub const SESSION_KEY_V2: u8 = 2;
const SESSION_KEY_AAD: &[u8] = b"krowk/session-key/v2";

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
pub(crate) fn id(label: &[u8], bytes: &[u8]) -> [u8; 16] {
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

/// The format byte of a sealed chunk.
pub const CHUNK_V1: u8 = 1;
/// A chunk's flag byte: this is the last chunk of the log.
const CHUNK_FINAL: u8 = 1;
/// `version | suite | flags | index (8) | fence (8)`, both big-endian, then
/// the nonce.
const CHUNK_HEAD: usize = 3 + 8 + 8;
/// What sealing adds to a chunk's plaintext.
pub const CHUNK_OVERHEAD: usize = CHUNK_HEAD + NONCE + TAG;
/// What chunk 0 binds as the chunk before it.
pub const NO_PREVIOUS_CHUNK: [u8; 32] = [0; 32];

/// A session's chunk epoch: fixed by the session key, so every device that
/// holds the key agrees on it without storing it, and a chunk sealed under
/// another session's key never has this chain's associated data.
fn chunk_epoch(key: &SessionKey) -> [u8; 16] {
    id(b"krowk/chunk-epoch/v1", &key.0)
}

/// The digest a chunk is chained by: SHA-256 of the whole sealed blob.
pub fn chunk_digest(blob: &[u8]) -> [u8; 32] {
    Sha256::digest(blob).into()
}

fn chunk_aad(head: &[u8], session: &[u8; 16], epoch: &[u8; 16], previous: &[u8; 32]) -> Vec<u8> {
    [&b"krowk/chunk/v1"[..], head, session, epoch, previous].concat()
}

/// A session's log at rest, sealed chunk by chunk for R2 (crypto.md →
/// chunks at rest). The chunk's index is its counter, the epoch is the
/// session's own (derived from the session key), the lease fence it was
/// written under is in its header, and it binds the digest of the chunk
/// before it — so the log is a chain: two holders' chunks cannot be spliced
/// into one log, nor one of two chunks at an index swapped for the other,
/// without the next chunk failing to open. The last chunk says it is the
/// last. `version | suite | flags | index | fence | nonce | ciphertext +
/// tag`, the associated data a label, those first nineteen bytes, the
/// session id, the epoch and the previous chunk's digest.
pub struct ChunkSealer {
    key: SessionKey,
    session: [u8; 16],
    next: u64,
    previous: [u8; 32],
    fence: u64,
    finished: bool,
}

impl ChunkSealer {
    /// Sealing from index `next` after the chunk whose digest is `previous`
    /// (`NO_PREVIOUS_CHUNK` for a new log), under lease `fence`. A holder
    /// taking up a log another device wrote reads it to the end first
    /// (`ChunkReader::next`, `ChunkReader::previous`), so it chains onto the
    /// log as it is and not as it last saw it.
    pub fn new(key: &SessionKey, session: [u8; 16], next: u64, previous: [u8; 32], fence: u64) -> ChunkSealer {
        ChunkSealer { key: key.clone(), session, next, previous, fence, finished: false }
    }

    /// The index the next chunk is sealed at.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Seals the next chunk; `last` ends the log, and nothing seals after it.
    pub fn seal(&mut self, plaintext: &[u8], last: bool) -> Result<Vec<u8>, Error> {
        if self.finished {
            return Err(err("this log has been ended by its final chunk; nothing is sealed after it"));
        }
        let head = [&[CHUNK_V1, SUITE_XCHACHA20_POLY1305, if last { CHUNK_FINAL } else { 0 }][..], &self.next.to_be_bytes(), &self.fence.to_be_bytes()].concat();
        let nonce: [u8; NONCE] = random();
        let aad = chunk_aad(&head, &self.session, &chunk_epoch(&self.key), &self.previous);
        let sealed = cipher(&self.key.0).encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: &aad }).map_err(|_| err("the chunk is too large to seal"))?;
        let blob = [&head[..], &nonce, &sealed].concat();
        self.next = self.next.checked_add(1).ok_or_else(|| err("a session's chunk index ran out"))?;
        self.previous = chunk_digest(&blob);
        self.finished = last;
        Ok(blob)
    }
}

/// Reads a session's log back, chunk by chunk, in order: index 0 first and
/// each one after it exactly one more, each bound to the one before it, and
/// its fence never lower than the last one's — so a repeated, reordered,
/// skipped, spliced or forked chunk is refused rather than opened out of
/// place, and nothing is opened after the final chunk. `finished` says
/// whether the final one arrived. A log without it may simply still be
/// being written: a reader cannot tell a live log from one a hostile
/// registry served only a prefix of (crypto.md → chunks at rest).
pub struct ChunkReader {
    key: SessionKey,
    session: [u8; 16],
    next: u64,
    previous: [u8; 32],
    fence: u64,
    finished: bool,
}

impl ChunkReader {
    pub fn new(key: &SessionKey, session: [u8; 16]) -> ChunkReader {
        ChunkReader { key: key.clone(), session, next: 0, previous: NO_PREVIOUS_CHUNK, fence: 0, finished: false }
    }

    /// Reading from chunk `next` on, the one after the chunk whose digest is
    /// `previous`, written under a fence no lower than `fence`: a viewer
    /// attaching from a checkpoint, where the session's sealed index says
    /// where the checkpoint sits and what came before it. The chain holds
    /// from there exactly as from 0. A holder that will write never starts
    /// here: it reads from 0, so what it chains onto is the whole log.
    pub fn resume(key: &SessionKey, session: [u8; 16], next: u64, previous: [u8; 32], fence: u64) -> ChunkReader {
        ChunkReader { key: key.clone(), session, next, previous, fence, finished: false }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The index the next chunk must have.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// The fence of the last chunk opened: the next may not be lower.
    pub fn fence(&self) -> u64 {
        self.fence
    }

    /// The digest the next chunk must bind: what a holder resuming the log
    /// hands its `ChunkSealer`.
    pub fn previous(&self) -> [u8; 32] {
        self.previous
    }

    pub fn open(&mut self, blob: &[u8]) -> Result<Vec<u8>, Error> {
        let refused = || err("the chunk does not open here: it was changed, belongs to another session, or does not follow the chunk before it");
        if blob.len() < CHUNK_OVERHEAD || blob[1] != SUITE_XCHACHA20_POLY1305 || blob[0] != CHUNK_V1 || blob[2] & !CHUNK_FINAL != 0 {
            return Err(match blob.first() {
                Some(&v) if v > CHUNK_V1 => err(format!("the chunk is format {v}, newer than this krowk reads — upgrade krowk")),
                _ => refused(),
            });
        }
        if self.finished {
            return Err(err("a chunk arrived after the log's final chunk, refused"));
        }
        let index = u64::from_be_bytes(blob[3..11].try_into().expect("eight bytes"));
        if index != self.next {
            return Err(err(format!("chunk {index} arrived where chunk {} belongs: a replay, a reordering or a gap, refused", self.next)));
        }
        let fence = u64::from_be_bytes(blob[11..CHUNK_HEAD].try_into().expect("eight bytes"));
        if fence < self.fence {
            return Err(err(format!("chunk {index} was written under lease {fence}, before the chunk ahead of it ({}), refused", self.fence)));
        }
        let nonce: [u8; NONCE] = blob[CHUNK_HEAD..CHUNK_HEAD + NONCE].try_into().expect("24 bytes");
        let aad = chunk_aad(&blob[..CHUNK_HEAD], &self.session, &chunk_epoch(&self.key), &self.previous);
        let plain = cipher(&self.key.0).decrypt(&XNonce::from(nonce), Payload { msg: &blob[CHUNK_HEAD + NONCE..], aad: &aad }).map_err(|_| refused())?;
        // Only an authentic chunk moves the reader on.
        self.next += 1;
        self.previous = chunk_digest(blob);
        self.fence = fence;
        self.finished = blob[2] & CHUNK_FINAL != 0;
        Ok(plain)
    }
}

fn cipher(key: &[u8; KEY]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&(*key).into())
}

/// The session key wrapped under its owner's user key, at the generation
/// current when it was sealed (engineering/devices.md → Keys): `version (2)
/// | suite | generation (4, BE) | nonce (20) | sealed key + tag` — 74
/// bytes, the size the registry holds a wrapped session key to.
///
/// - The **generation is in the clear**, so an opener knows which
///   generation to walk `UserKeys` down to, and a device that holds only
///   older ones can say so rather than "does not open".
/// - The XChaCha20 nonce is the 24 bytes from the generation on: the
///   generation and 20 random bytes. 160 random bits leave a repeat under
///   one generation out of reach, and the generation is sealed over twice.
/// - The associated data is `"krowk/session-key/v2"` ‖ version ‖ suite ‖
///   generation ‖ the user key's id ‖ the session's 16-byte id. A blob
///   moved to another session, or relabelled as another generation, does
///   not open.
pub fn wrap_session_key(session_key: &SessionKey, session: &[u8; 16], user: &UserKey) -> Vec<u8> {
    let mut blob = Vec::with_capacity(WRAPPED_SESSION_KEY);
    blob.extend_from_slice(&[SESSION_KEY_V2, SUITE_XCHACHA20_POLY1305]);
    blob.extend_from_slice(&user.generation().to_be_bytes());
    blob.extend_from_slice(&random::<{ NONCE - 4 }>());
    let nonce: [u8; NONCE] = blob[2..2 + NONCE].try_into().expect("24 bytes");
    let aad = session_key_aad(&blob[..2], user, session);
    let sealed = cipher(user.as_bytes()).encrypt(&XNonce::from(nonce), Payload { msg: &session_key.0, aad: &aad }).expect("sealing 32 bytes cannot fail");
    blob.extend_from_slice(&sealed);
    blob
}

/// A new session's key wrapped under the newest generation this device
/// holds, refused unless that is the generation the verified device chain
/// names as current (`current_generation`, from `Chain::generation`). A
/// device behind a rotation — offline, or fed a stale list — would
/// otherwise seal under a generation a removed device still holds.
pub fn seal_session_key(session_key: &SessionKey, session: &[u8; 16], keys: &UserKeys, current_generation: u32) -> Result<Vec<u8>, Error> {
    let held = keys.newest().generation();
    if held < current_generation {
        return Err(err(format!("this device holds user key generation {held}, but the device list names generation {current_generation} — nothing is sealed under an old key; catch this device up with the device list (`krowk devices`) and try again")));
    }
    Ok(wrap_session_key(session_key, session, keys.newest()))
}

/// The user key generation a wrapped session key names, read before
/// anything is opened. Refused for a blob that is not one.
pub fn session_key_generation(blob: &[u8]) -> Result<u32, Error> {
    if blob.len() != WRAPPED_SESSION_KEY || blob[0] != SESSION_KEY_V2 || blob[1] != SUITE_XCHACHA20_POLY1305 {
        return Err(match blob.first() {
            Some(&v) if v > SESSION_KEY_V2 => newer("session", v),
            _ => err("the wrapped session key is not one this krowk reads: it was changed, or was sealed before session keys were sealed under the user key"),
        });
    }
    match u32::from_be_bytes(blob[2..6].try_into().expect("four bytes")) {
        0 => Err(err("the wrapped session key names user key generation 0, which does not exist")),
        g => Ok(g),
    }
}

/// The session key out of a blob `wrap_session_key` made, opened with the
/// generation it names — the newest, or an older one reached down the
/// chain of wraps. A generation newer than any this device holds is
/// refused as such: a removed device holds none made after its removal.
pub fn unwrap_session_key(blob: &[u8], session: &[u8; 16], keys: &UserKeys) -> Result<SessionKey, Error> {
    let generation = session_key_generation(blob)?;
    let held = keys.newest().generation();
    if generation > held {
        return Err(err(format!("the session key is sealed under user key generation {generation}, newer than generation {held} this device holds — it was removed from your devices, or has not taken up the new key yet")));
    }
    let user = keys.open(generation)?;
    let refused = || err("the wrapped session key does not open with this user key: it was changed, or belongs to another session or person");
    let nonce: [u8; NONCE] = blob[2..2 + NONCE].try_into().expect("24 bytes");
    let aad = session_key_aad(&blob[..2], &user, session);
    let mut plain = cipher(user.as_bytes()).decrypt(&XNonce::from(nonce), Payload { msg: &blob[2 + NONCE..], aad: &aad }).map_err(|_| refused())?;
    let out = <[u8; KEY]>::try_from(&plain[..]).map(SessionKey);
    plain.zeroize();
    out.map_err(|_| refused())
}

fn session_key_aad(head: &[u8], user: &UserKey, session: &[u8; 16]) -> Vec<u8> {
    [SESSION_KEY_AAD, head, &user.generation().to_be_bytes(), &user.id().0, session].concat()
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
pub use crate::protocol::frame::HEADER;
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

    /// The counter the next message is sealed with.
    pub fn next_counter(&self) -> u64 {
        self.next
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

    /// A chain whose epoch the sender picked and announced: the host's
    /// stream across the relay, taken only from an announcement sealed with
    /// this receiver's own challenge bound in, and a viewer's first frame,
    /// whose epoch both ends derive from its link (`relay_link`). Not public:
    /// an epoch a receiver did not pick opens recordings, unless something
    /// bound to this connection vouched for it.
    pub(crate) fn for_epoch(key: &SessionKey, session: [u8; 16], direction: Direction, epoch: [u8; 16]) -> Opener {
        Opener { key: key.clone(), session, direction, epoch, last: None }
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

/// The label a relay join's signature starts with (engineering/relay.md).
pub const RELAY_JOIN_LABEL: &[u8] = b"krowk/relay/v1";

/// The roles a device joins a relay channel as, as the signature binds them.
pub const RELAY_ROLE_HOST: u8 = 1;
pub const RELAY_ROLE_VIEWER: u8 = 2;

/// A device's relay signing key: Ed25519, beside its X25519 device key,
/// which can agree on a key but cannot sign. It proves to a relay which
/// device is connecting, and nothing else: it wraps no key and seals no
/// content, so a relay that learns every signature learns who connected,
/// which it knows anyway. Wiped when dropped.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    pub fn generate() -> SigningKey {
        SigningKey(ed25519_dalek::SigningKey::from_bytes(&random::<32>()))
    }

    /// The key from its stored 32-byte seed.
    pub fn from_secret(bytes: &[u8]) -> Result<SigningKey, Error> {
        let seed = zeroize::Zeroizing::new(<[u8; 32]>::try_from(bytes).map_err(|_| err("the signing key is not a 32-byte Ed25519 seed"))?);
        Ok(SigningKey(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    /// The seed, for the keystore to write. Wiped when dropped.
    pub fn secret_bytes(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.0.to_bytes())
    }

    pub fn public(&self) -> SigningPublic {
        SigningPublic(self.0.verifying_key().to_bytes())
    }

    /// Answers a relay's challenge: a signature over `relay_join_message`.
    /// `origin` is the relay the caller dialed, as it dialed it — never the
    /// origin a relay's challenge names, or a relay could pass another
    /// relay's challenge through and join there as this device. It is
    /// signed in its canonical form (`canonical_origin`); a URL that has
    /// none is refused rather than signed as it came.
    pub fn sign_relay_join(&self, role: u8, session: &[u8; 16], nonce: &[u8; 32], device: &DeviceId, origin: &str) -> Result<[u8; 64], Error> {
        use ed25519_dalek::Signer as _;
        let origin = canonical_origin(origin).ok_or_else(|| err(format!("{origin:?} is not a relay URL (ws://, wss://, http:// or https:// and a host)")))?;
        Ok(self.0.sign(&relay_join_message(role, session, nonce, device, &origin)).to_bytes())
    }
}

/// A device's signing key, signing the registry calls that act as the
/// device (canon, engineering/crypto.md → Signed registry requests): what
/// `krowk_api::Client::signed_by` takes. krowk-api builds the lines; the
/// key and the signature stay here (R-E2E-2). The lines begin with their
/// own label, `krowk/registry/v1`, so a registry signature is never a
/// relay join's and one is never the other's.
pub struct DeviceSigner {
    device: DeviceId,
    key: SigningKey,
}

impl DeviceSigner {
    pub fn new(device: DeviceId, key: SigningKey) -> DeviceSigner {
        DeviceSigner { device, key }
    }

    /// This signer, as the registry client takes one.
    pub fn shared(self) -> std::sync::Arc<dyn krowk_api::client::RequestSigner> {
        std::sync::Arc::new(self)
    }
}

impl krowk_api::client::RequestSigner for DeviceSigner {
    fn device(&self) -> String {
        self.device.to_string()
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.key.0.sign(message).to_bytes()
    }
}

/// Checks a signed registry call's signature, strictly, as the registry
/// does: for the stand-in registry the client's tests run against.
pub fn verify_registry_request(signing_key: &[u8], signature: &[u8], message: &[u8]) -> bool {
    let (Ok(key), Ok(sig)) = (<[u8; 32]>::try_from(signing_key), <[u8; 64]>::try_from(signature)) else { return false };
    ed25519_dalek::VerifyingKey::from_bytes(&key).is_ok_and(|k| k.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok())
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SigningKey({})", hex(&self.public().0))
    }
}

/// A device's Ed25519 public key: what the registry's device record, and
/// so the relay, holds to check a join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigningPublic(pub [u8; 32]);

impl SigningPublic {
    /// Whether this is an Ed25519 key anything can verify with: a point of
    /// the curve, in its one canonical encoding, not of small order. The
    /// registry refuses any other on a device list entry
    /// (`Sync.refuse_weak_signing_key!`), and so does the verifier.
    pub fn is_strong(&self) -> bool {
        ed25519_dalek::VerifyingKey::from_bytes(&self.0).is_ok_and(|k| !k.is_weak() && k.to_edwards().compress().to_bytes() == self.0)
    }

    /// Checks a join's signature, strictly (RFC 8032's checks and no
    /// small-order or non-canonical keys or signatures), so one signature
    /// has one encoding and a key cannot be chosen to verify anything.
    pub fn verify_relay_join(&self, signature: &[u8], role: u8, session: &[u8; 16], nonce: &[u8; 32], device: &DeviceId, origin: &str) -> Result<(), Error> {
        let bad = || err("the signature does not verify");
        let origin = canonical_origin(origin).ok_or_else(bad)?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&self.0).map_err(|_| bad())?;
        let sig: [u8; 64] = signature.try_into().map_err(|_| bad())?;
        key.verify_strict(&relay_join_message(role, session, nonce, device, &origin), &ed25519_dalek::Signature::from_bytes(&sig)).map_err(|_| bad())
    }
}

/// A relay's origin as a join signs it: `ws://` or `wss://` (`http` and
/// `https` map to them, since a Worker sees an upgrade as HTTP), the host
/// lowercased, an IPv6 address in brackets, the port only when it is not
/// the scheme's default (80, 443), and nothing after the authority. The
/// signer and the verifier each reduce what they have to this, so `wss://
/// Relay.krowk.com:443/v1/relay/…` and `https://relay.krowk.com` sign alike.
/// None for anything else, a user name or an empty host included.
pub fn canonical_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (scheme, default) = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => ("ws", 80),
        "wss" | "https" => ("wss", 443),
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (h, after) = inner.split_once(']')?;
        // RFC 5952's text, so every spelling of one address signs alike.
        let ip = h.parse::<std::net::Ipv6Addr>().ok()?;
        (format!("[{ip}]"), after.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), Some(p)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() || (host.contains(':') && !host.starts_with('[')) || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-[]:".contains(&b)) {
        return None;
    }
    let port = match port {
        None | Some("") => None,
        Some(p) => Some(p.parse::<u16>().ok().filter(|_| p.bytes().all(|b| b.is_ascii_digit()))?),
    };
    Some(match port {
        Some(p) if p != default => format!("{scheme}://{host}:{p}"),
        _ => format!("{scheme}://{host}"),
    })
}

/// What a relay join signs: `"krowk/relay/v1" ‖ role (1) ‖ session (16) ‖
/// nonce (32) ‖ device id (16) ‖ origin`, the origin last so its length
/// needs no prefix.
pub fn relay_join_message(role: u8, session: &[u8; 16], nonce: &[u8; 32], device: &DeviceId, origin: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(RELAY_JOIN_LABEL.len() + 65 + origin.len());
    m.extend_from_slice(RELAY_JOIN_LABEL);
    m.push(role);
    m.extend_from_slice(session);
    m.extend_from_slice(nonce);
    m.extend_from_slice(&device.0);
    m.extend_from_slice(origin.as_bytes());
    m
}

// The primitives the user key (`user_key`) and the device chain
// (`device_chain`) are built on, kept here so the suite and the key types'
// private halves stay in this one module.

/// HPKE Base-mode seal to a device, in the pinned suite: `(enc, sealed)`.
pub(crate) fn hpke_seal(plain: &[u8], to: &DevicePublic, info: &[u8], aad: &[u8]) -> Result<([u8; 32], Vec<u8>), Error> {
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(&to.0).map_err(|_| err("the device's public key is not an X25519 key"))?;
    let (enc, sealed) = hpke::single_shot_seal::<HpkeAead, Kdf, Kem>(&OpModeS::Base, &pk, info, plain, aad).map_err(|_| err("the key could not be wrapped"))?;
    Ok((enc.to_bytes().into(), sealed))
}

/// HPKE Base-mode open with this device's private key. None for anything
/// that does not open; the caller says what it was.
pub(crate) fn hpke_open(device: &DeviceKey, enc: &[u8], sealed: &[u8], info: &[u8], aad: &[u8]) -> Option<zeroize::Zeroizing<Vec<u8>>> {
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc).ok()?;
    hpke::single_shot_open::<HpkeAead, Kdf, Kem>(&OpModeR::Base, &device.secret, &enc, info, sealed, aad).ok().map(zeroize::Zeroizing::new)
}

/// RFC 9180's DeriveKeyPair: the same keypair from the same 32 bytes, for
/// a device whose key is derived rather than drawn (the recovery device).
pub(crate) fn derive_device_key(ikm: &[u8; 32]) -> DeviceKey {
    let (secret, public) = Kem::derive_keypair(ikm);
    DeviceKey { secret, public }
}

impl SigningKey {
    /// An Ed25519 signature over `message`, which the caller has already
    /// prefixed with its own label.
    pub(crate) fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.0.sign(message).to_bytes()
    }
}

impl SigningPublic {
    /// Checks a signature strictly, as `verify_relay_join` does.
    pub(crate) fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
        let bad = || err("the signature does not verify");
        let key = ed25519_dalek::VerifyingKey::from_bytes(&self.0).map_err(|_| bad())?;
        key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature)).map_err(|_| bad())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-RELAY-1: a join's signature verifies only for its own role,
    /// session, nonce, device and origin, and only under its own key.
    #[test]
    fn r_relay_1_a_join_signature_binds_role_session_nonce_device_and_origin() {
        let k = SigningKey::generate();
        let again = SigningKey::from_secret(&k.secret_bytes()[..]).unwrap();
        assert_eq!(again.public(), k.public());
        let (s, n, d) = ([7u8; 16], [9u8; 32], DeviceId([3u8; 16]));
        let sig = k.sign_relay_join(RELAY_ROLE_VIEWER, &s, &n, &d, "wss://relay.krowk.com").unwrap();
        let p = k.public();
        p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, "wss://relay.krowk.com").unwrap();
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_HOST, &s, &n, &d, "wss://relay.krowk.com").is_err(), "a viewer's signature is not a host's");
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &[8u8; 16], &n, &d, "wss://relay.krowk.com").is_err());
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &[1u8; 32], &d, "wss://relay.krowk.com").is_err());
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &DeviceId([4u8; 16]), "wss://relay.krowk.com").is_err());
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, "wss://evil.example").is_err(), "another relay's challenge does not pass through");
        assert!(SigningKey::generate().public().verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, "wss://relay.krowk.com").is_err());
        assert!(p.verify_relay_join(&sig[..63], RELAY_ROLE_VIEWER, &s, &n, &d, "wss://relay.krowk.com").is_err());
        assert!(SigningPublic([0u8; 32]).verify_relay_join(&[0u8; 64], RELAY_ROLE_VIEWER, &s, &n, &d, "wss://x").is_err(), "a small-order key verifies nothing");
        // The origin is signed canonically: the forms a Worker, a proxy and
        // a client may each hold of one relay verify alike.
        for same in ["wss://Relay.Krowk.com", "wss://relay.krowk.com:443", "https://relay.krowk.com/v1/relay/x", "WSS://relay.krowk.com/"] {
            p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, same).unwrap();
        }
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, "ws://relay.krowk.com").is_err(), "the scheme is signed");
        assert!(p.verify_relay_join(&sig, RELAY_ROLE_VIEWER, &s, &n, &d, "wss://relay.krowk.com:8443").is_err());
        assert!(k.sign_relay_join(RELAY_ROLE_VIEWER, &s, &n, &d, "relay.krowk.com").is_err());
    }

    #[test]
    fn r_relay_1_origins_are_canonical() {
        let c = |u: &str| canonical_origin(u);
        assert_eq!(c("ws://127.0.0.1:80").as_deref(), Some("ws://127.0.0.1"));
        assert_eq!(c("http://LOCALHOST:7790/x?y").as_deref(), Some("ws://localhost:7790"));
        assert_eq!(c("wss://[::1]:443").as_deref(), Some("wss://[::1]"));
        assert_eq!(c("ws://[::1]:7790").as_deref(), Some("ws://[::1]:7790"));
        assert_eq!(c("ws://[0:0:0:0:0:0:0:1]:7790").as_deref(), Some("ws://[::1]:7790"));
        assert_eq!(c("wss://[2001:DB8:0:0:0:0:0:1]").as_deref(), Some("wss://[2001:db8::1]"));
        for bad in ["ftp://x", "ws://", "ws://user@host", "ws://h:port", "ws://h:99999", "ws://[zz]", "ws://a:b:c", "ws://h o"] {
            assert_eq!(c(bad), None, "{bad}");
        }
    }

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

    /// Generations 1 to `n`, and every generation's `UserKeys` as a device
    /// holding it would have them: the key and the wraps beneath it.
    fn generations(n: usize) -> (Vec<UserKey>, Vec<UserKeys>) {
        let mut keys = vec![UserKey::first()];
        while keys.len() < n {
            keys.push(keys.last().unwrap().next().unwrap());
        }
        let held = (0..n).map(|top| UserKeys::new(keys[top].clone(), (0..top).map(|g| keys[g + 1].wrap_previous(&keys[g]).unwrap())).unwrap()).collect();
        (keys, held)
    }

    #[test]
    fn r_e2e_2_d8_session_keys_wrap_under_the_user_key_and_refuse_tampering() {
        let (users, held) = generations(1);
        let key = SessionKey::generate();
        let blob = wrap_session_key(&key, &SESSION, &users[0]);
        assert_eq!(blob.len(), WRAPPED_SESSION_KEY);
        assert_eq!(blob.len(), 74, "the size the registry holds a wrapped session key to");
        assert_eq!(unwrap_session_key(&blob, &SESSION, &held[0]).unwrap().as_bytes(), key.as_bytes());
        for i in 0..blob.len() {
            assert!(unwrap_session_key(&flip(&blob, i), &SESSION, &held[0]).is_err(), "byte {i} changed and it still opened");
        }
        assert!(unwrap_session_key(&blob, b"another-session!", &held[0]).is_err(), "it opened for another session");
        let later = [&[3u8][..], &blob[1..]].concat();
        assert!(unwrap_session_key(&later, &SESSION, &held[0]).unwrap_err().0.contains("format 3, newer"));
        // A blob sealed under the account key, before the user key, is not
        // read as one: clean break, no fallback.
        let before = [&[1u8][..], &blob[1..]].concat();
        assert!(!unwrap_session_key(&before, &SESSION, &held[0]).unwrap_err().0.contains("upgrade"));
        let (_, stranger) = generations(1);
        assert!(unwrap_session_key(&blob, &SESSION, &stranger[0]).is_err(), "another person's user key opened it");
    }

    /// D8: a session sealed under an older generation opens with the newest
    /// through the chain of wraps, and records the generation it was
    /// sealed under.
    #[test]
    fn d8_a_session_sealed_under_an_older_generation_opens_with_the_newest() {
        let (users, held) = generations(4);
        for (g, user) in users.iter().enumerate() {
            let key = SessionKey::generate();
            let blob = wrap_session_key(&key, &SESSION, user);
            assert_eq!(session_key_generation(&blob).unwrap(), user.generation());
            for newer in &held[g..] {
                assert_eq!(unwrap_session_key(&blob, &SESSION, newer).unwrap().as_bytes(), key.as_bytes(), "generation {} opened with {}", g + 1, newer.newest().generation());
            }
        }
    }

    /// D8: a device holding only an older generation — one removed before
    /// the rotation, or not yet caught up — does not open a session sealed
    /// under a newer one, and is told which it would need.
    #[test]
    fn d8_a_device_holding_only_an_older_generation_does_not_open_a_newer_session() {
        let (users, held) = generations(3);
        let blob = wrap_session_key(&SessionKey::generate(), &SESSION, &users[2]);
        for older in &held[..2] {
            let e = unwrap_session_key(&blob, &SESSION, older).unwrap_err().0;
            assert!(e.contains("generation 3") && e.contains(&format!("generation {} this device holds", older.newest().generation())), "{e}");
        }
        // Relabelled as the older generation it holds, it still does not
        // open: the generation is sealed over, and it is not the key.
        let mut relabelled = blob.clone();
        relabelled[2..6].copy_from_slice(&2u32.to_be_bytes());
        assert!(unwrap_session_key(&relabelled, &SESSION, &held[1]).is_err());
        // Nor does a same-generation key that is not the person's.
        let impostor = UserKeys::new(UserKey::from_bytes(3, [9; 32]).unwrap(), []).unwrap();
        assert!(unwrap_session_key(&blob, &SESSION, &impostor).is_err());
        let zero = [&blob[..2], &[0u8; 4][..], &blob[6..]].concat();
        assert!(session_key_generation(&zero).is_err());
    }

    /// D8 (M2 of #200's review): a device behind the generation the chain
    /// names seals nothing.
    #[test]
    fn d8_a_device_behind_the_chains_generation_seals_nothing() {
        let (_, held) = generations(2);
        let key = SessionKey::generate();
        let e = seal_session_key(&key, &SESSION, &held[0], 2).unwrap_err().0;
        assert!(e.contains("generation 1") && e.contains("names generation 2") && e.contains("catch this device up"), "{e}");
        let blob = seal_session_key(&key, &SESSION, &held[1], 2).unwrap();
        assert_eq!(session_key_generation(&blob).unwrap(), 2);
    }

    /// Frozen: a later change to the layout or the associated data makes
    /// this stop opening, and this fails.
    #[test]
    fn d8_a_frozen_wrapped_session_key_still_opens() {
        let g1 = UserKey::from_bytes(1, [0x31; 32]).unwrap();
        let g2 = UserKey::from_bytes(2, [0x32; 32]).unwrap();
        let keys = UserKeys::new(g2.clone(), [g2.wrap_previous(&g1).unwrap()]).unwrap();
        let blob = unhex(FROZEN_SESSION_KEY).unwrap();
        assert_eq!(session_key_generation(&blob).unwrap(), 1);
        assert_eq!(unwrap_session_key(&blob, &SESSION, &keys).unwrap().as_bytes(), &[0x44; 32]);
    }

    const FROZEN_SESSION_KEY: &str = "02020000000102fac5e5821af44c433077886dd5064cb4127206cbbd97a4f3e2cd340ed8557920e83e03594ff840ad9484f58b2a608ee8d5ab34db729345407613b047695957c6150836";

    /// Prints a fresh blob for the frozen test above: run once with
    /// `--ignored --nocapture` when the format is meant to change.
    #[test]
    #[ignore]
    fn print_frozen_wrapped_session_key() {
        let g1 = UserKey::from_bytes(1, [0x31; 32]).unwrap();
        println!("session_key {}", hex(&wrap_session_key(&SessionKey([0x44; 32]), &SESSION, &g1)));
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
        // 0 is "none" in the header (protocol::frame::ENC_NONE); this is the
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
    /// R-E2E-1: a session's log at rest opens in order under its session key,
    /// and a replayed, reordered, skipped, moved or changed chunk does not.
    #[test]
    fn r_e2e_1_chunks_open_in_order_and_only_for_their_session() {
        let key = SessionKey::generate();
        let mut sealer = ChunkSealer::new(&key, SESSION, 0, NO_PREVIOUS_CHUNK, 1);
        let (a, b, c) = (sealer.seal(b"turn one", false).unwrap(), sealer.seal(b"turn two", false).unwrap(), sealer.seal(b"end", true).unwrap());
        assert!(sealer.seal(b"after", false).is_err(), "nothing seals after the final chunk");
        assert!(!a.windows(4).any(|w| w == b"turn"), "the plaintext is not in the chunk");

        let mut reader = ChunkReader::new(&key, SESSION);
        assert_eq!(reader.open(&a).unwrap(), b"turn one");
        assert!(reader.open(&a).is_err(), "a replay");
        assert!(reader.open(&c).unwrap_err().0.contains("chunk 2 arrived where chunk 1 belongs"), "a gap");
        assert!(!reader.finished());
        assert_eq!(reader.open(&b).unwrap(), b"turn two");
        assert_eq!(reader.open(&c).unwrap(), b"end");
        assert!(reader.finished());

        assert!(ChunkReader::new(&key, *b"fedcba9876543210").open(&a).is_err(), "moved to another session");
        assert!(ChunkReader::new(&SessionKey::generate(), SESSION).open(&a).is_err(), "another session's key");
        let mut changed = a.clone();
        changed[2] = CHUNK_FINAL;
        assert!(ChunkReader::new(&key, SESSION).open(&changed).is_err(), "the final flag is bound");
        let mut refenced = a.clone();
        refenced[18] ^= 1;
        assert!(ChunkReader::new(&key, SESSION).open(&refenced).is_err(), "the fence is bound");
        let mut newer = a;
        newer[0] = CHUNK_V1 + 1;
        assert!(ChunkReader::new(&key, SESSION).open(&newer).unwrap_err().0.contains("upgrade krowk"));
    }

    /// R-SYNC-2: a holder taking up a log chains onto it as it stands, and a
    /// reader reads the two holders' chunks as one log — while a displaced
    /// holder's chunk at the same index, or a fork, does not splice in.
    #[test]
    fn r_sync_2_the_log_is_a_chain_across_holders_and_a_splice_does_not_open() {
        let key = SessionKey::generate();
        let mut a = ChunkSealer::new(&key, SESSION, 0, NO_PREVIOUS_CHUNK, 1);
        let zero = a.seal(b"zero", false).unwrap();
        // A writes chunk 1, then loses the lease before it reaches the log.
        let a_one = a.seal(b"A's one", false).unwrap();

        // B read the log to its end — chunk 0 — and chains onto that.
        let mut resumed = ChunkReader::new(&key, SESSION);
        resumed.open(&zero).unwrap();
        let mut b = ChunkSealer::new(&key, SESSION, resumed.next(), resumed.previous(), 2);
        let b_one = b.seal(b"B's one", false).unwrap();
        let b_two = b.seal(b"B's two", true).unwrap();

        let mut reader = ChunkReader::new(&key, SESSION);
        for (blob, text) in [(&zero, &b"zero"[..]), (&b_one, b"B's one"), (&b_two, b"B's two")] {
            assert_eq!(reader.open(blob).unwrap(), text);
        }
        assert!(reader.finished());

        // A's chunk 1 followed by B's chunk 2: B's does not follow A's.
        let mut spliced = ChunkReader::new(&key, SESSION);
        spliced.open(&zero).unwrap();
        spliced.open(&a_one).unwrap();
        assert!(spliced.open(&b_two).is_err(), "a splice of two holders' logs");

        // A chunk from an earlier lease after a later one's is refused.
        let mut late = ChunkReader::new(&key, SESSION);
        let mut c = ChunkSealer::new(&key, SESSION, 0, NO_PREVIOUS_CHUNK, 5);
        late.open(&c.seal(b"five", false).unwrap()).unwrap();
        let mut d = ChunkSealer::new(&key, SESSION, 1, late.previous(), 4);
        assert!(late.open(&d.seal(b"four", false).unwrap()).unwrap_err().0.contains("before the chunk ahead of it"));
    }
}
