//! The user key: 32 random bytes per person, not per workspace, that every
//! private session key is wrapped under (engineering/devices.md in Canon →
//! Keys). It carries a generation that starts at 1 and goes up by one on
//! every rotation.
//!
//! - Its **id** is the first 16 bytes of `SHA-256("krowk/user-key-id/v1" ‖
//!   generation (u32 BE) ‖ key)`, so two generations never share an id even
//!   if their bytes did.
//! - Each generation is **wrapped to a device** with the same HPKE suite as
//!   the account key (`e2e`): `version | suite | generation (4) | id (16) |
//!   enc (32) | sealed key + tag` (102 bytes). Version, suite, generation
//!   and id are the associated data; `info` is `"krowk/user-key/v1" ‖ id ‖
//!   generation ‖ device id`, so a blob moved to another device, or passed
//!   off as another generation, does not open.
//! - **HPKE Base mode does not say who wrapped a key**: anyone holding a
//!   device's public key — the registry — can wrap a key of its own to it.
//!   So a device adopts generation g only when the opened key's computed id
//!   is the id its verified device chain commits to for g (`unwrap` takes
//!   that id, never the blob header's), and, when it already holds an
//!   older generation, only when the new one opens back down to the one it
//!   holds (`UserKeys::adopt`).
//! - Each generation g+1 **wraps generation g** with XChaCha20-Poly1305:
//!   `version | suite | generation g (4) | id g (16) | nonce (24) | sealed
//!   key + tag` (94 bytes). The associated data is `"krowk/user-key-chain/v1"
//!   ‖ version ‖ suite ‖ id g ‖ g ‖ id g+1 ‖ g+1`, so a wrap opens only under
//!   the very next generation and only as the generation it names.
//! - **`UserKeys`** holds the newest generation and those wraps, and opens
//!   any older generation by walking down one step at a time.
//!
//! Every refusal reads "does not open"; every secret is wiped when dropped.

use crate::e2e::{self, DeviceKey, DevicePublic, Error};
use chacha20poly1305::aead::{Aead as _, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

const KEY: usize = 32;
const NONCE: usize = 24;
const TAG: usize = 16;
const HEAD: usize = 2 + 4 + 16;

/// A user key wrapped to one device.
pub const WRAPPED_USER_KEY: usize = HEAD + 32 + KEY + TAG;
/// Generation g wrapped under generation g+1.
pub const WRAPPED_PREVIOUS: usize = HEAD + NONCE + KEY + TAG;

const WRAP_INFO: &[u8] = b"krowk/user-key/v1";
const CHAIN_AAD: &[u8] = b"krowk/user-key-chain/v1";

/// A user key's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserKeyId(pub [u8; 16]);

impl std::fmt::Display for UserKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&e2e::hex(&self.0))
    }
}

/// One generation of the user key. Wiped when dropped; never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct UserKey {
    generation: u32,
    key: [u8; KEY],
}

impl UserKey {
    /// Generation 1: a person's first user key.
    pub fn first() -> UserKey {
        UserKey { generation: 1, key: e2e::random() }
    }

    /// The generation after this one, for a rotation. Refused at the end of
    /// `u32`, which no person reaches.
    pub fn next(&self) -> Result<UserKey, Error> {
        let generation = self.generation.checked_add(1).ok_or_else(|| Error("the user key has no generation after this one".into()))?;
        Ok(UserKey { generation, key: e2e::random() })
    }

    /// A key from its bytes, as a test vector or a stored key gives it.
    /// Generation 0 is not one that exists.
    pub fn from_bytes(generation: u32, key: [u8; KEY]) -> Result<UserKey, Error> {
        if generation == 0 {
            return Err(Error("user key generations start at 1".into()));
        }
        Ok(UserKey { generation, key })
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn as_bytes(&self) -> &[u8; KEY] {
        &self.key
    }

    pub fn id(&self) -> UserKeyId {
        id(self.generation, &self.key)
    }

    /// This generation wrapped to one device (`WRAPPED_USER_KEY` bytes).
    pub fn wrap_to(&self, device: &DevicePublic) -> Result<Vec<u8>, Error> {
        let head = head(e2e::SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305, self.generation, self.id());
        let (enc, sealed) = e2e::hpke_seal(&self.key, device, &wrap_info(self.id(), self.generation, device.id()), &head)?;
        Ok([&head[..], &enc, &sealed].concat())
    }

    /// The user key out of a blob `wrap_to` made for this device, refused
    /// unless it is generation `generation` and its computed id is `expected`
    /// — both from the verified device chain (`Chain::key_id_at`). The
    /// header's id is the sender's claim, so it is never what is checked.
    pub fn unwrap(blob: &[u8], generation: u32, expected: UserKeyId, device: &DeviceKey) -> Result<UserKey, Error> {
        let refused = || Error("the wrapped user key does not open with this device's key: it was changed, or was wrapped for another device or generation".into());
        if blob.len() != WRAPPED_USER_KEY || blob[0] != e2e::BLOB_V1 || blob[1] != e2e::SUITE_HPKE_X25519_SHA256_CHACHA20POLY1305 {
            return Err(match blob.first() {
                Some(&v) if v > e2e::BLOB_V1 => newer(v),
                _ => refused(),
            });
        }
        let (g, key_id) = parse_head(blob);
        if g != generation || g == 0 || key_id != expected {
            return Err(refused());
        }
        let info = wrap_info(expected, g, device.id());
        let plain = e2e::hpke_open(device, &blob[HEAD..HEAD + 32], &blob[HEAD + 32..], &info, &blob[..HEAD]).ok_or_else(refused)?;
        let key = UserKey { generation: g, key: <[u8; KEY]>::try_from(&plain[..]).map_err(|_| refused())? };
        if key.id() != expected {
            return Err(refused());
        }
        Ok(key)
    }

    /// The generation before this one, wrapped under this one
    /// (`WRAPPED_PREVIOUS` bytes). Refused unless `older` is exactly one
    /// generation back.
    pub fn wrap_previous(&self, older: &UserKey) -> Result<Vec<u8>, Error> {
        if older.generation.checked_add(1) != Some(self.generation) {
            return Err(Error(format!("generation {} wraps only generation {}, not {}", self.generation, self.generation - 1, older.generation)));
        }
        let head = head(e2e::SUITE_XCHACHA20_POLY1305, older.generation, older.id());
        let nonce: [u8; NONCE] = e2e::random();
        let aad = chain_aad(&head, self);
        let sealed = cipher(&self.key).encrypt(&XNonce::from(nonce), Payload { msg: &older.key, aad: &aad }).expect("sealing 32 bytes cannot fail");
        Ok([&head[..], &nonce, &sealed].concat())
    }

    /// The generation before this one, out of a blob `wrap_previous` made.
    pub fn unwrap_previous(&self, blob: &[u8]) -> Result<UserKey, Error> {
        let refused = || Error("the wrapped earlier user key does not open with this generation: it was changed, or belongs to another generation".into());
        if blob.len() != WRAPPED_PREVIOUS || blob[0] != e2e::BLOB_V1 || blob[1] != e2e::SUITE_XCHACHA20_POLY1305 {
            return Err(match blob.first() {
                Some(&v) if v > e2e::BLOB_V1 => newer(v),
                _ => refused(),
            });
        }
        let (g, key_id) = parse_head(blob);
        if g == 0 || g.checked_add(1) != Some(self.generation) {
            return Err(refused());
        }
        let nonce: [u8; NONCE] = blob[HEAD..HEAD + NONCE].try_into().expect("24 bytes");
        let aad = chain_aad(&blob[..HEAD], self);
        let mut plain = cipher(&self.key).decrypt(&XNonce::from(nonce), Payload { msg: &blob[HEAD + NONCE..], aad: &aad }).map_err(|_| refused())?;
        let out = <[u8; KEY]>::try_from(&plain[..]);
        plain.zeroize();
        let key = UserKey { generation: g, key: out.map_err(|_| refused())? };
        if key.id() != key_id {
            return Err(refused());
        }
        Ok(key)
    }
}

impl PartialEq for UserKey {
    /// In constant time over the key bytes, as `AccountKey`'s.
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation && self.key.iter().zip(other.key.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

impl std::fmt::Debug for UserKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UserKey(generation {}, {})", self.generation, self.id())
    }
}

/// The newest generation and the wraps of every older one under the next,
/// opening any generation back to 1.
pub struct UserKeys {
    newest: UserKey,
    /// `wraps[g]`: generation g wrapped under g+1, as the registry holds it.
    wraps: std::collections::BTreeMap<u32, Vec<u8>>,
}

impl UserKeys {
    /// The keys from the newest generation and the wraps of older ones, in
    /// any order. A wrap is keyed by the generation its header names; each
    /// is checked only when it is opened, so a bad one refuses only the
    /// generations beneath it. Two wraps naming one generation are refused:
    /// a server offering both is shadowing one of them.
    pub fn new(newest: UserKey, wraps: impl IntoIterator<Item = Vec<u8>>) -> Result<UserKeys, Error> {
        let mut map = std::collections::BTreeMap::new();
        for w in wraps {
            if w.len() != WRAPPED_PREVIOUS {
                return Err(Error("a wrapped earlier user key is not one this krowk reads".into()));
            }
            if map.insert(parse_head(&w).0, w).is_some() {
                return Err(Error("two wraps name the same user key generation".into()));
            }
        }
        Ok(UserKeys { newest, wraps: map })
    }

    /// A newer generation, taken up by a device that holds `held`: the
    /// device's wrap of generation `generation` must open to the id the
    /// verified chain commits to (`expected`), and the new keys must open
    /// back down to exactly `held`. Only then is anything sealed under it.
    pub fn adopt(held: &UserKey, blob: &[u8], generation: u32, expected: UserKeyId, device: &DeviceKey, wraps: impl IntoIterator<Item = Vec<u8>>) -> Result<UserKeys, Error> {
        if generation <= held.generation {
            return Err(Error(format!("user key generation {generation} is not newer than generation {} held", held.generation)));
        }
        let keys = UserKeys::new(UserKey::unwrap(blob, generation, expected, device)?, wraps)?;
        if keys.open(held.generation)? != *held {
            return Err(Error("the new user key does not lead back to the one this device holds — refused".into()));
        }
        Ok(keys)
    }

    pub fn newest(&self) -> &UserKey {
        &self.newest
    }

    /// Generation `generation`, opened by walking down from the newest.
    pub fn open(&self, generation: u32) -> Result<UserKey, Error> {
        if generation == 0 || generation > self.newest.generation {
            return Err(Error(format!("there is no user key generation {generation}")));
        }
        let mut key = self.newest.clone();
        while key.generation > generation {
            let below = key.generation - 1;
            let wrap = self.wraps.get(&below).ok_or_else(|| Error(format!("the wrap of user key generation {below} is missing")))?;
            key = key.unwrap_previous(wrap)?;
        }
        Ok(key)
    }
}

fn id(generation: u32, key: &[u8; KEY]) -> UserKeyId {
    UserKeyId(e2e::id(b"krowk/user-key-id/v1", &[&generation.to_be_bytes()[..], key].concat()))
}

fn head(suite: u8, generation: u32, key: UserKeyId) -> [u8; HEAD] {
    let mut h = [0u8; HEAD];
    h[0] = e2e::BLOB_V1;
    h[1] = suite;
    h[2..6].copy_from_slice(&generation.to_be_bytes());
    h[6..22].copy_from_slice(&key.0);
    h
}

fn parse_head(blob: &[u8]) -> (u32, UserKeyId) {
    (u32::from_be_bytes(blob[2..6].try_into().expect("four bytes")), UserKeyId(blob[6..22].try_into().expect("sixteen bytes")))
}

fn wrap_info(key: UserKeyId, generation: u32, device: e2e::DeviceId) -> Vec<u8> {
    [WRAP_INFO, &key.0, &generation.to_be_bytes(), &device.0].concat()
}

fn chain_aad(head: &[u8], newer: &UserKey) -> Vec<u8> {
    [CHAIN_AAD, &head[..2], &head[6..22], &head[2..6], &newer.id().0, &newer.generation.to_be_bytes()].concat()
}

fn cipher(key: &[u8; KEY]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&(*key).into())
}

fn newer(v: u8) -> Error {
    Error(format!("the wrapped user key is format {v}, newer than this krowk reads — upgrade krowk"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answer, frozen: the id is SHA-256 over the label, the
    /// generation big-endian and the key, cut to 16 bytes.
    #[test]
    fn d1_the_user_key_id_binds_its_generation_known_answer() {
        let one = UserKey::from_bytes(1, [0x42; 32]).unwrap();
        let two = UserKey::from_bytes(2, [0x42; 32]).unwrap();
        assert_eq!(one.id().to_string(), "44a301e5d533c38411bc62b3f83654bb");
        assert_ne!(one.id(), two.id());
        assert!(UserKey::from_bytes(0, [0; 32]).is_err());
    }

    #[test]
    fn d1_a_user_key_opens_only_on_its_device_and_as_its_generation() {
        let (laptop, other) = (DeviceKey::generate(), DeviceKey::generate());
        let key = UserKey::first();
        let blob = key.wrap_to(&laptop.public()).unwrap();
        assert_eq!(blob.len(), WRAPPED_USER_KEY);
        assert_eq!(UserKey::unwrap(&blob, 1, key.id(), &laptop).unwrap(), key);
        assert!(UserKey::unwrap(&blob, 1, key.id(), &other).is_err(), "another device");
        assert!(UserKey::unwrap(&blob, 2, key.id(), &laptop).is_err(), "another generation expected");
        // The header's generation and id are bound: changing either refuses.
        for at in [0, 1, 5, 6, 21, 40, WRAPPED_USER_KEY - 1] {
            let mut bad = blob.clone();
            bad[at] ^= 1;
            assert!(UserKey::unwrap(&bad, 1, key.id(), &laptop).is_err(), "byte {at} changed");
        }
    }

    #[test]
    fn d1_each_generation_wraps_only_the_one_before_it() {
        let g1 = UserKey::first();
        let g2 = g1.next().unwrap();
        let g3 = g2.next().unwrap();
        assert_eq!(g3.generation(), 3);
        let w1 = g2.wrap_previous(&g1).unwrap();
        assert_eq!(w1.len(), WRAPPED_PREVIOUS);
        assert_eq!(g2.unwrap_previous(&w1).unwrap(), g1);
        assert!(g3.wrap_previous(&g1).is_err(), "a skipped generation is refused");
        assert!(g3.unwrap_previous(&w1).is_err(), "only the next generation opens it");
        // A same-generation impostor with other bytes does not open it.
        let impostor = UserKey::from_bytes(2, [7; 32]).unwrap();
        assert!(impostor.unwrap_previous(&w1).is_err());
        for at in [1, 2, 5, 6, 30, WRAPPED_PREVIOUS - 1] {
            let mut bad = w1.clone();
            bad[at] ^= 1;
            assert!(g2.unwrap_previous(&bad).is_err(), "byte {at} changed");
        }
    }

    #[test]
    fn d1_the_newest_user_key_opens_every_older_generation() {
        let mut keys = vec![UserKey::first()];
        for _ in 0..4 {
            keys.push(keys.last().unwrap().next().unwrap());
        }
        let wraps: Vec<Vec<u8>> = keys.windows(2).rev().map(|w| w[1].wrap_previous(&w[0]).unwrap()).collect();
        let all = UserKeys::new(keys[4].clone(), wraps.clone()).unwrap();
        for k in &keys {
            assert_eq!(&all.open(k.generation()).unwrap(), k);
        }
        assert!(all.open(0).is_err());
        assert!(all.open(6).is_err());
        // A missing link refuses every generation beneath it, and only those.
        let gapped = UserKeys::new(keys[4].clone(), wraps.clone().into_iter().filter(|w| parse_head(w).0 != 3)).unwrap();
        assert!(gapped.open(4).is_ok());
        assert!(gapped.open(3).is_err());
        assert!(gapped.open(1).is_err());
        // A second wrap for one generation shadows the first: refused.
        let mut twice = wraps.clone();
        twice.push(keys[3].wrap_previous(&keys[2]).unwrap());
        assert!(UserKeys::new(keys[4].clone(), twice).is_err());
    }

    /// C1 (review of #199): HPKE Base mode lets anyone holding a device's
    /// public key wrap a key to it, so the registry can offer its own key
    /// as the next generation. The chain's key id refuses it, and so does
    /// the check that the new generation leads back to the one held.
    #[test]
    fn d1_a_user_key_wrapped_by_the_registry_is_refused() {
        let phone = DeviceKey::generate();
        let g1 = UserKey::first();
        let g2 = g1.next().unwrap();
        let forged = UserKey::from_bytes(2, [0xaa; 32]).unwrap();
        let blob = forged.wrap_to(&phone.public()).unwrap();
        assert!(UserKey::unwrap(&blob, 2, g2.id(), &phone).is_err(), "the chain commits to the real id");
        // Even offered with the forged id, the forged key does not lead back.
        let link = forged.wrap_previous(&UserKey::from_bytes(1, [0xbb; 32]).unwrap()).unwrap();
        assert!(UserKeys::adopt(&g1, &blob, 2, forged.id(), &phone, [link]).is_err());
        // The genuine rotation is adopted.
        let real = g2.wrap_to(&phone.public()).unwrap();
        let keys = UserKeys::adopt(&g1, &real, 2, g2.id(), &phone, [g2.wrap_previous(&g1).unwrap()]).unwrap();
        assert_eq!(keys.newest(), &g2);
        assert!(UserKeys::adopt(&g2, &real, 2, g2.id(), &phone, []).is_err(), "not newer");
    }

    /// Frozen blobs from this format: a later change to the layout, the
    /// labels or the suite makes them stop opening, and this fails.
    #[test]
    fn d1_frozen_user_key_wraps_still_open() {
        let device = DeviceKey::from_secret(&[0x11; 32]).unwrap();
        let g1 = UserKey::from_bytes(1, [0x21; 32]).unwrap();
        let g2 = UserKey::from_bytes(2, [0x22; 32]).unwrap();
        let to_device = e2e::unhex(FROZEN_TO_DEVICE).unwrap();
        let previous = e2e::unhex(FROZEN_PREVIOUS).unwrap();
        assert_eq!(UserKey::unwrap(&to_device, 2, g2.id(), &device).unwrap(), g2);
        assert_eq!(g2.unwrap_previous(&previous).unwrap(), g1);
    }

    const FROZEN_TO_DEVICE: &str = "0101000000023acc064f6f4dcde4a28ffba4be4505e06e10feb29895d9edfd17d66063674715fd8b7e1e01fb49c545a4b18ee2ff4230fa0bf750f85cf3601efa3f2223e0c0ce11c0fc3985720cde566edd6311dc9b44c36c7f9c11b29da900a22f212cf07c97";
    const FROZEN_PREVIOUS: &str = "0102000000014e14c03d5d8a06093a6162f57d556c64fa0c6e915fa0b1f643bc9ef34b4019be26a98ed348a286beba7de71fd5b7cb0ec07ba7ff2f3028cc54daca8f026316fb1182f2bcf3be097ab3e9703eec1ef5781a3771c4c30017f1";

    /// Prints fresh blobs for the frozen test above: run once with
    /// `--ignored --nocapture` when the format is meant to change.
    #[test]
    #[ignore]
    fn print_frozen_user_key_wraps() {
        let device = DeviceKey::from_secret(&[0x11; 32]).unwrap();
        let g1 = UserKey::from_bytes(1, [0x21; 32]).unwrap();
        let g2 = UserKey::from_bytes(2, [0x22; 32]).unwrap();
        println!("to_device {}", e2e::hex(&g2.wrap_to(&device.public()).unwrap()));
        println!("previous {}", e2e::hex(&g2.wrap_previous(&g1).unwrap()));
    }
}
