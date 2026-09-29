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
    let mut bits = Zeroizing::new([0u8; 33]);
    bits[..32].copy_from_slice(key.as_bytes());
    bits[32] = Sha256::digest(key.as_bytes())[0];
    let words = list();
    let mut out = Zeroizing::new(String::with_capacity(WORDS * 9));
    for i in 0..WORDS {
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

/// The key back from its words: any case, any whitespace between them.
/// Refused with the word or the count that is wrong, or the checksum — never
/// echoing the phrase.
pub fn decode(phrase: &str) -> Result<AccountKey, String> {
    let words = list();
    let mut lower = Zeroizing::new(phrase.to_lowercase());
    let given: Vec<&str> = lower.split_whitespace().collect();
    if given.len() != WORDS {
        return Err(format!("a recovery phrase is {WORDS} words, and this is {} — enter every word, in order", given.len()));
    }
    let mut bits = Zeroizing::new([0u8; 33]);
    for (i, w) in given.iter().enumerate() {
        let n = words.binary_search(w).map_err(|_| format!("word {} is not one a recovery phrase uses — check its spelling", i + 1))?;
        for k in 0..11 {
            if n >> (10 - k) & 1 == 1 {
                let b = i * 11 + k;
                bits[b / 8] |= 1 << (7 - b % 8);
            }
        }
    }
    drop(given);
    lower.zeroize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&bits[..32]);
    let ok = Sha256::digest(key)[0] == bits[32];
    let account = AccountKey::from_bytes(key);
    key.zeroize();
    if !ok {
        return Err("the recovery phrase does not check out — a word is wrong or two are swapped; check each against where you wrote it down".into());
    }
    Ok(account)
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
}
