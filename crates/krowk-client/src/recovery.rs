//! The recovery kit and the recovery device it stands for
//! (engineering/devices.md in Canon → Keys, Recovery).
//!
//! The kit is 128 random bits, shown as 12 words (`phrase::encode_kit`).
//! It is not the user key: it derives a device — an X25519 keypair and an
//! Ed25519 signing key — that sits on the device list with kind `recovery`
//! and has the user key wrapped to it on every rotation, like any other.
//! So a rotation never makes the kit stale, and replacing the kit needs no
//! change to anything sealed.
//!
//! The derivation: HKDF-SHA256 with no salt, the kit's 16 bytes as the
//! input key material and `"krowk/recovery-device/v1"` as `info`, expanded
//! to 64 bytes. The first 32 are RFC 9180's DeriveKeyPair input for the
//! X25519 key; the last 32 are the Ed25519 seed. The same words give the
//! same device on every machine, which is how `krowk sync recover` finds it
//! on the chain.

use crate::e2e::{self, DeviceKey, SigningKey};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const INFO: &[u8] = b"krowk/recovery-device/v1";

/// A recovery kit's 128 bits. Wiped when dropped; never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct RecoveryKit([u8; 16]);

impl RecoveryKit {
    /// A new kit, from the operating system's random source.
    pub fn generate() -> RecoveryKit {
        RecoveryKit(e2e::random())
    }

    pub fn from_bytes(bytes: [u8; 16]) -> RecoveryKit {
        RecoveryKit(bytes)
    }

    /// The kit back from its 12 words (`phrase::decode_kit`'s refusals).
    pub fn from_words(words: &str) -> Result<RecoveryKit, String> {
        crate::phrase::decode_kit(words).map(|b| RecoveryKit(*b))
    }

    /// The kit as its 12 words, for the person to write down.
    pub fn words(&self) -> Zeroizing<String> {
        crate::phrase::encode_kit(&self.0)
    }

    /// The recovery device this kit derives.
    pub fn device(&self) -> RecoveryDevice {
        // HKDF's pseudorandom key lives in the HMAC-SHA-256 state inside
        // `hk`; sha2's `zeroize` feature (Cargo.toml) wipes that state when
        // `hk` is dropped, here at the end of this block. Every copy of the
        // output is wiped the same way.
        let mut okm = Zeroizing::new([0u8; 64]);
        {
            let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, &self.0);
            hk.expand(INFO, &mut okm[..]).expect("64 bytes is within HKDF-SHA256's limit");
        }
        let mut ikm = Zeroizing::new([0u8; 32]);
        ikm.copy_from_slice(&okm[..32]);
        let key = e2e::derive_device_key(&ikm);
        let signing = SigningKey::from_secret(&okm[32..]).expect("a 32-byte seed");
        RecoveryDevice { key, signing }
    }
}

impl std::fmt::Debug for RecoveryKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKit(…)")
    }
}

/// The recovery device's two keys, both wiped when dropped.
#[derive(Debug)]
pub struct RecoveryDevice {
    pub key: DeviceKey,
    pub signing: SigningKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answer, frozen: the public keys the all-zero kit and a kit of
    /// 0x01..0x10 derive. A change to the label, the split or the
    /// derivation moves them, and every kit ever written stops working.
    #[test]
    fn d1_the_kit_derives_its_recovery_device_known_answer() {
        let cases: [([u8; 16], &str, &str); 2] = [
            ([0; 16], "99431817513a8a27a56fef4349664cd4cfaada795fd8c5fa1e5720d05d554f3b", "dce4476f44aa387f22d70ae566ba8619e62aa418fc878b6c99166d5a80f5c4f3"),
            (core::array::from_fn(|i| i as u8 + 1), "d6f91da4b504fe762ef2ff8a334d679fb97aceb38486dd12223c994ec0b72756", "f1af1b746a2c3da55e4f0cbb5877bfe852d8a20b1562a2036a57b52ff15f3abf"),
        ];
        for (bytes, x25519, ed25519) in cases {
            let d = RecoveryKit::from_bytes(bytes).device();
            assert_eq!(e2e::hex(&d.key.public().0), x25519);
            assert_eq!(e2e::hex(&d.signing.public().0), ed25519);
        }
    }

    #[test]
    fn d1_the_same_words_give_the_same_recovery_device() {
        let kit = RecoveryKit::generate();
        let again = RecoveryKit::from_words(&kit.words()).unwrap();
        let (a, b) = (kit.device(), again.device());
        assert_eq!(a.key.public(), b.key.public());
        assert_eq!(a.signing.public(), b.signing.public());
        let other = RecoveryKit::generate().device();
        assert_ne!(a.key.public(), other.key.public());
        assert_ne!(a.signing.public(), other.signing.public());
    }

    /// Prints the known answers above: run once with `--ignored
    /// --nocapture` when the derivation is meant to change.
    #[test]
    #[ignore]
    fn print_recovery_known_answer() {
        for bytes in [[0u8; 16], core::array::from_fn(|i| i as u8 + 1)] {
            let d = RecoveryKit::from_bytes(bytes).device();
            println!("kat {} {}", e2e::hex(&d.key.public().0), e2e::hex(&d.signing.public().0));
        }
    }
}
