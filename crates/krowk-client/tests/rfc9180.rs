//! R-E2E-2: the HPKE suite krowk wraps account keys with, held to RFC 9180's
//! own test vector for it — Appendix A.2.1, Base mode,
//! DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305 — in both
//! directions. tests/data/rfc9180-a2-base.json is that one entry of the
//! RFC's test-vectors.json, its first eight encryptions and its exports.
//!
//! The sender side is deterministic here because the hpke crate draws an
//! ephemeral key as DeriveKeyPair(random(32)): an RNG that returns the
//! vector's `ikmE` makes it the vector's ephemeral key, so `enc` and every
//! ciphertext must come out byte for byte.
//!
//! R-OSS-1: the public crypto design, `engineering/crypto.md` in Canon,
//! states the HPKE crate's audit status on the strength of this file — no
//! paid audit, and these vectors passing — so a change here is a change
//! to what that document may claim.

use hpke::rand_core::{TryCryptoRng, TryRng};
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use krowk_client::e2e::{HpkeAead, Kdf, Kem};
use serde_json::Value;
use std::convert::Infallible;

fn vector() -> Value {
    serde_json::from_str(include_str!("data/rfc9180-a2-base.json")).unwrap()
}

fn bytes(v: &Value, k: &str) -> Vec<u8> {
    krowk_client::e2e::unhex(v[k].as_str().unwrap()).unwrap()
}

/// Returns the bytes it was given, then nothing: one ephemeral key's worth.
struct Fixed(Vec<u8>);

impl TryRng for Fixed {
    type Error = Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        unreachable!("only fill_bytes is asked for")
    }
    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        unreachable!("only fill_bytes is asked for")
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        assert_eq!(dst.len(), self.0.len(), "asked for other than one ikmE");
        dst.copy_from_slice(&std::mem::take(&mut self.0));
        Ok(())
    }
}

impl TryCryptoRng for Fixed {}

#[test]
fn r_e2e_2_the_suite_is_the_one_the_vector_names() {
    let v = vector();
    assert_eq!((v["mode"].as_u64(), v["kem_id"].as_u64(), v["kdf_id"].as_u64(), v["aead_id"].as_u64()), (Some(0), Some(0x20), Some(1), Some(3)));
}

#[test]
fn r_e2e_2_rfc_9180_vectors_derive_the_recipient_key() {
    let v = vector();
    let (sk, pk) = Kem::derive_keypair(&bytes(&v, "ikmR"));
    assert_eq!(sk.to_bytes().as_slice(), bytes(&v, "skRm"));
    assert_eq!(pk.to_bytes().as_slice(), bytes(&v, "pkRm"));
    let (ske, pke) = Kem::derive_keypair(&bytes(&v, "ikmE"));
    assert_eq!(ske.to_bytes().as_slice(), bytes(&v, "skEm"));
    assert_eq!(pke.to_bytes().as_slice(), bytes(&v, "pkEm"));
}

#[test]
fn r_e2e_2_rfc_9180_vectors_seal() {
    let v = vector();
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(&bytes(&v, "pkRm")).unwrap();
    let (enc, mut ctx) = hpke::setup_sender_with_rng::<HpkeAead, Kdf, Kem>(&OpModeS::Base, &pk, &bytes(&v, "info"), &mut Fixed(bytes(&v, "ikmE"))).unwrap();
    assert_eq!(enc.to_bytes().as_slice(), bytes(&v, "enc"));
    for (i, e) in v["encryptions"].as_array().unwrap().iter().enumerate() {
        assert_eq!(ctx.seal(&bytes(e, "pt"), &bytes(e, "aad")).unwrap(), bytes(e, "ct"), "encryption {i}");
    }
}

#[test]
fn r_e2e_2_rfc_9180_vectors_open_and_export() {
    let v = vector();
    let sk = <Kem as hpke::Kem>::PrivateKey::from_bytes(&bytes(&v, "skRm")).unwrap();
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&bytes(&v, "enc")).unwrap();
    let mut ctx = hpke::setup_receiver::<HpkeAead, Kdf, Kem>(&OpModeR::Base, &sk, &enc, &bytes(&v, "info")).unwrap();
    let encryptions = v["encryptions"].as_array().unwrap();
    assert_eq!(encryptions.len(), 8);
    for (i, e) in encryptions.iter().enumerate() {
        assert_eq!(ctx.open(&bytes(e, "ct"), &bytes(e, "aad")).unwrap(), bytes(e, "pt"), "encryption {i}");
    }
    let exports = v["exports"].as_array().unwrap();
    assert_eq!(exports.len(), 3);
    for e in exports {
        let mut out = vec![0u8; e["L"].as_u64().unwrap() as usize];
        ctx.export(&bytes(e, "exporter_context"), &mut out).unwrap();
        assert_eq!(out, bytes(e, "exported_value"));
    }
}

/// The opened side refuses a ciphertext of the vector with one bit changed.
#[test]
fn r_e2e_2_rfc_9180_a_changed_ciphertext_does_not_open() {
    let v = vector();
    let sk = <Kem as hpke::Kem>::PrivateKey::from_bytes(&bytes(&v, "skRm")).unwrap();
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&bytes(&v, "enc")).unwrap();
    let mut ctx = hpke::setup_receiver::<HpkeAead, Kdf, Kem>(&OpModeR::Base, &sk, &enc, &bytes(&v, "info")).unwrap();
    let e = &v["encryptions"][0];
    let mut ct = bytes(e, "ct");
    ct[0] ^= 1;
    assert!(ctx.open(&ct, &bytes(e, "aad")).is_err());
}
