//! The recovery kit and the recovery device it stands for
//! (engineering/devices.md in Canon → Keys, Recovery).
//!
//! The kit is 128 random bits, shown as 12 words: BIP39's encoding — the
//! bits, then the first 4 bits of their SHA-256 as a checksum, cut into 12
//! groups of 11 bits, each an index into BIP39's English list — and nothing
//! else of BIP39: no PBKDF2 seed, no passphrase. The checksum catches a word
//! typed wrong 15 times in 16; the rest are caught when the derived
//! recovery device is not on the device list. The words are wiped when
//! dropped, and never written anywhere unless the person asks for a file.
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
use sha2::Digest as _;
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

    /// The kit back from its 12 words: any case, any whitespace between
    /// them. Refused with the word or the count that is wrong, or the
    /// checksum — never echoing the words.
    pub fn from_words(words: &str) -> Result<RecoveryKit, String> {
        from_words(words).map(|b| RecoveryKit(*b))
    }

    /// The kit as its 12 words, for the person to write down.
    pub fn words(&self) -> Zeroizing<String> {
        to_words(&self.0)
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

/// BIP39's English list (bitcoin/bips, bip-0039/english.txt, MIT): 2048
/// words, sorted, each unique in its first four letters.
const ENGLISH: &str = include_str!("english.txt");

/// The words in a kit.
pub const KIT_WORDS: usize = 12;

fn list() -> &'static [&'static str] {
    static LIST: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| ENGLISH.lines().collect())
}

/// The kit's 16 bytes, then their SHA-256's first byte (whose top 4 bits
/// are the checksum), read 11 bits at a time.
fn to_words(kit: &[u8; 16]) -> Zeroizing<String> {
    let mut bits = Zeroizing::new(kit.to_vec());
    bits.push(sha2::Sha256::digest(kit)[0]);
    let mut out = Zeroizing::new(String::with_capacity(KIT_WORDS * 9));
    for i in 0..KIT_WORDS {
        let n = (i * 11..i * 11 + 11).fold(0usize, |n, b| (n << 1) | ((bits[b / 8] >> (7 - b % 8)) & 1) as usize);
        if i > 0 {
            out.push(' ');
        }
        out.push_str(list()[n]);
    }
    out
}

fn from_words(words: &str) -> Result<Zeroizing<[u8; 16]>, String> {
    let mut lower = Zeroizing::new(words.to_lowercase());
    let given: Vec<&str> = lower.split_whitespace().collect();
    if given.len() != KIT_WORDS {
        return Err(format!("a recovery kit is {KIT_WORDS} words, and this is {} — enter every word, in order", given.len()));
    }
    let mut bits = Zeroizing::new([0u8; 17]);
    for (i, w) in given.iter().enumerate() {
        let n = list().binary_search(w).map_err(|_| format!("word {} is not one a recovery kit uses — check its spelling", i + 1))?;
        for k in 0..11 {
            if n >> (10 - k) & 1 == 1 {
                let b = i * 11 + k;
                bits[b / 8] |= 1 << (7 - b % 8);
            }
        }
    }
    drop(given);
    lower.zeroize();
    let mut kit = Zeroizing::new([0u8; 16]);
    kit.copy_from_slice(&bits[..16]);
    if sha2::Sha256::digest(*kit)[0] & 0xf0 != bits[16] & 0xf0 {
        return Err("the recovery kit does not check out — a word is wrong or two are swapped; check each against where you wrote it down".into());
    }
    Ok(kit)
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

    #[test]
    fn the_list_is_bip39_english_exactly() {
        let hash = sha2::Sha256::digest(ENGLISH.as_bytes());
        assert_eq!(crate::e2e::hex(&hash), "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda");
        assert_eq!(list().len(), 2048);
        assert!(list().windows(2).all(|w| w[0] < w[1]), "sorted, for the binary search");
    }

    /// BIP39's own vectors for 128-bit entropy (trezor/python-mnemonic's
    /// vectors.json), so a kit written by another BIP39 tool from the same
    /// bytes reads the same.
    #[test]
    fn d1_the_recovery_kit_is_bip39s_12_word_encoding_of_128_bits() {
        let cases: [([u8; 16], &str); 4] = [
            ([0; 16], "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"),
            ([0x7f; 16], "legal winner thank year wave sausage worth useful legal winner thank yellow"),
            ([0x80; 16], "letter advice cage absurd amount doctor acoustic avoid letter advice cage above"),
            ([0xff; 16], "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong"),
        ];
        for (bytes, words) in cases {
            assert_eq!(&*to_words(&bytes), words);
            assert_eq!(*from_words(words).unwrap(), bytes);
        }
        for _ in 0..64 {
            let kit: [u8; 16] = crate::e2e::random();
            let words = to_words(&kit);
            assert_eq!(words.split(' ').count(), KIT_WORDS);
            assert_eq!(*from_words(&words.to_uppercase()).unwrap(), kit);
        }
    }

    /// `about` → `abandon` as the last word of the all-zero kit changes only
    /// the 4 checksum bits (0011 → 0000), so the checksum refuses it.
    #[test]
    fn d1_a_wrong_word_in_the_kit_is_refused() {
        let right = to_words(&[0; 16]);
        let swapped = right.replace(" about", " abandon");
        let e = from_words(&swapped).unwrap_err();
        assert!(e.contains("recovery kit does not check out"), "{e}");
        assert!(!e.contains("abandon"));
        assert!(from_words(right.rsplit_once(' ').unwrap().0).unwrap_err().contains("12 words, and this is 11"));
        let misspelt = right.replacen("abandon", "abandn", 1);
        assert_eq!(from_words(&misspelt).unwrap_err(), "word 1 is not one a recovery kit uses — check its spelling");
    }
}
