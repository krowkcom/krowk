//! The recovery phrase: the account key itself as 24 English words
//! (R-E2E-4). BIP39's encoding — 256 bits of entropy, then the first byte
//! of their SHA-256 as a checksum, cut into 24 groups of 11 bits, each an
//! index into BIP39's English list — and nothing else of BIP39: no PBKDF2
//! seed, no passphrase. The phrase *is* the key, so restoring it gives back
//! exactly the key every session was wrapped under.
//!
//! The checksum is 8 bits: a mistyped word that is still a list word gets
//! through one time in 256, as a valid phrase for another key. Nothing
//! local can tell — until the registry holds the account's key id (ticket
//! 16) — so `krowk sync init` shows the key id beside the phrase and
//! `recover` shows the id it restored, for the person to compare. A word
//! not on the list is caught every time.
//!
//! The recovery kit (`encode_kit`, `decode_kit`) is the same encoding over
//! 128 bits: 12 words, with BIP39's 4-bit checksum. It replaces the phrase
//! once the device chain ships (engineering/devices.md in Canon); the
//! 24-word phrase stays until the CLI moves over.
//!
//! The phrase is never written to disk or to a log: the caller shows it and
//! drops it, and every copy here is wiped when dropped.

use crate::e2e::AccountKey;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use zeroize::{Zeroize, Zeroizing};

/// BIP39's English list (bitcoin/bips, bip-0039/english.txt, MIT): 2048
/// words, sorted, each unique in its first four letters.
const ENGLISH: &str = include_str!("english.txt");

pub const WORDS: usize = 24;

fn list() -> &'static [&'static str] {
    static LIST: OnceLock<Vec<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| ENGLISH.lines().collect())
}

/// The key as its 24 words, one space between each.
pub fn encode(key: &AccountKey) -> Zeroizing<String> {
    to_words(key.as_bytes())
}

/// The key back from its words: any case, any whitespace between them.
/// Refused with the word or the count that is wrong, or the checksum — never
/// echoing the phrase.
pub fn decode(phrase: &str) -> Result<AccountKey, String> {
    let mut bytes = [0u8; 32];
    from_words(phrase, &mut bytes, "recovery phrase")?;
    let account = AccountKey::from_bytes(bytes);
    bytes.zeroize();
    Ok(account)
}

/// The words in a recovery kit (engineering/devices.md in Canon →
/// Recovery): BIP39's encoding of 128 bits, with its 4-bit checksum.
pub const KIT_WORDS: usize = 12;

/// A recovery kit's 128 bits as its 12 words.
pub fn encode_kit(kit: &[u8; 16]) -> Zeroizing<String> {
    to_words(kit)
}

/// A recovery kit's 128 bits back from its words, as `decode` reads a
/// phrase. The checksum catches a wrong word 15 times in 16; the rest are
/// caught when the derived recovery device is not on the device chain.
pub fn decode_kit(words: &str) -> Result<Zeroizing<[u8; 16]>, String> {
    let mut kit = Zeroizing::new([0u8; 16]);
    from_words(words, &mut kit[..], "recovery kit")?;
    Ok(kit)
}

/// BIP39's encoding of `entropy` (16 or 32 bytes): the bytes, then the
/// first `len × 8 / 32` bits of their SHA-256, in groups of 11 bits.
fn to_words(entropy: &[u8]) -> Zeroizing<String> {
    let count = (entropy.len() * 8 + entropy.len() / 4) / 11;
    let mut bits = Zeroizing::new(entropy.to_vec());
    bits.push(Sha256::digest(entropy)[0]);
    let words = list();
    let mut out = Zeroizing::new(String::with_capacity(count * 9));
    for i in 0..count {
        let mut n = 0usize;
        for b in i * 11..i * 11 + 11 {
            n = (n << 1) | ((bits[b / 8] >> (7 - b % 8)) & 1) as usize;
        }
        if i > 0 {
            out.push(' ');
        }
        out.push_str(words[n]);
    }
    out
}

/// The inverse of `to_words`, into `out`, whose length says how many words
/// to expect. `what` names the thing in the error.
fn from_words(phrase: &str, out: &mut [u8], what: &str) -> Result<(), String> {
    let count = (out.len() * 8 + out.len() / 4) / 11;
    let check_bits = out.len() / 4;
    let words = list();
    let mut lower = Zeroizing::new(phrase.to_lowercase());
    let given: Vec<&str> = lower.split_whitespace().collect();
    if given.len() != count {
        return Err(format!("a {what} is {count} words, and this is {} — enter every word, in order", given.len()));
    }
    let mut bits = Zeroizing::new(vec![0u8; out.len() + 1]);
    for (i, w) in given.iter().enumerate() {
        let n = words.binary_search(w).map_err(|_| format!("word {} is not one a {what} uses — check its spelling", i + 1))?;
        for k in 0..11 {
            if n >> (10 - k) & 1 == 1 {
                let b = i * 11 + k;
                bits[b / 8] |= 1 << (7 - b % 8);
            }
        }
    }
    drop(given);
    lower.zeroize();
    out.copy_from_slice(&bits[..out.len()]);
    let mask = !0xffu8.checked_shr(check_bits as u32).unwrap_or(0);
    if Sha256::digest(&*out)[0] & mask != bits[out.len()] & mask {
        out.zeroize();
        return Err(format!("the {what} does not check out — a word is wrong or two are swapped; check each against where you wrote it down"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: [u8; 32]) -> AccountKey {
        AccountKey::from_bytes(b)
    }

    #[test]
    fn the_list_is_bip39_english_exactly() {
        let hash = Sha256::digest(ENGLISH.as_bytes());
        assert_eq!(crate::e2e::hex(&hash), "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda");
        assert_eq!(list().len(), 2048);
        assert!(list().windows(2).all(|w| w[0] < w[1]), "sorted, for the binary search");
    }

    /// BIP39's own vectors for 256-bit entropy (trezor/python-mnemonic's
    /// vectors.json), so a phrase written by another BIP39 tool from the
    /// same bytes reads the same.
    #[test]
    fn r_e2e_4_the_phrase_is_bip39s_encoding_of_the_key() {
        let cases: [([u8; 32], &str); 4] = [
            ([0; 32], "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art"),
            ([0x7f; 32], "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title"),
            ([0x80; 32], "letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic bless"),
            ([0xff; 32], "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote"),
        ];
        for (bytes, words) in cases {
            assert_eq!(&*encode(&key(bytes)), words);
            assert_eq!(decode(words).unwrap(), key(bytes));
        }
    }

    #[test]
    fn r_e2e_4_a_random_key_round_trips_through_its_phrase() {
        for _ in 0..64 {
            let k = AccountKey::generate();
            let p = encode(&k);
            assert_eq!(p.split(' ').count(), WORDS);
            assert_eq!(decode(&p).unwrap(), k);
            assert_eq!(decode(&format!("  {}\n", p.to_uppercase().replace(' ', "\t "))).unwrap(), k);
        }
    }

    /// Deterministic: the all-zero key's last word is `art`; `abandon` in
    /// its place is a list word whose checksum bits are 0000 0000 against
    /// art's 0110 0110, so the checksum refuses it every run.
    #[test]
    fn r_e2e_4_a_typo_in_the_phrase_is_refused() {
        let right = encode(&key([0; 32]));
        let swapped = right.replace(" art", " abandon");
        assert!(decode(&swapped).unwrap_err().contains("does not check out"));
        let misspelt = right.replacen("abandon", "abandn", 1);
        assert_eq!(decode(&misspelt).unwrap_err(), "word 1 is not one a recovery phrase uses — check its spelling");
        let short = right.rsplit_once(' ').unwrap().0;
        assert!(decode(short).unwrap_err().contains("this is 23"));
        // The error never repeats the phrase.
        assert!(!decode(&swapped).unwrap_err().contains("abandon"));
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
            assert_eq!(&*encode_kit(&bytes), words);
            assert_eq!(*decode_kit(words).unwrap(), bytes);
        }
        for _ in 0..64 {
            let kit: [u8; 16] = crate::e2e::random();
            let words = encode_kit(&kit);
            assert_eq!(words.split(' ').count(), KIT_WORDS);
            assert_eq!(*decode_kit(&words.to_uppercase()).unwrap(), kit);
        }
    }

    /// `about` → `abandon` as the last word of the all-zero kit changes only
    /// the 4 checksum bits (0011 → 0000), so the checksum refuses it.
    #[test]
    fn d1_a_wrong_word_in_the_kit_is_refused() {
        let right = encode_kit(&[0; 16]);
        let swapped = right.replace(" about", " abandon");
        let e = decode_kit(&swapped).unwrap_err();
        assert!(e.contains("recovery kit does not check out"), "{e}");
        assert!(!e.contains("abandon"));
        assert!(decode_kit(&encode(&key([0; 32]))).unwrap_err().contains("12 words, and this is 24"));
        assert!(decode(&right).unwrap_err().contains("24 words, and this is 12"));
    }
}
