//! The release's own signature on `checksums.txt`, checked before
//! `krowk upgrade` trusts a line of it (R-OSS-3). The release workflow signs
//! it with cosign, keyless: GitHub's OIDC token names the workflow at its tag,
//! Fulcio certifies a one-use key for that name, and Rekor logs the signature.
//! What lands beside it is `checksums.txt.sigstore.json`, a Sigstore bundle,
//! and this checks it the way `cosign verify-blob` with SECURITY.md's identity
//! does, on what the lean build already links for TLS (ring through
//! rustls-webpki) rather than on sigstore-rs:
//!
//! - the certificate chains to Fulcio's root at the time Rekor logged the
//!   entry, for code signing — the certificate lives ten minutes, so the
//!   time is the log's, not now;
//! - its SAN is the release workflow at `refs/tags/v<version>`, the version
//!   being installed, its OIDC issuer is GitHub Actions', its source
//!   repository krowkcom/krowk and its trigger a push;
//! - its key signed these exact bytes;
//! - Rekor logged that signature with that certificate for this digest: its
//!   signed entry timestamp (which also vouches for the time above) and the
//!   inclusion proof up to a checkpoint Rekor signed for its own origin.
//!
//! What cosign also does and this does not: the certificate's SCT (Fulcio's
//! issuance in the CT log) and the RFC 3161 timestamp are not checked, and a
//! bundle logged only in Rekor v2 (no signed entry timestamp) is refused as
//! one this krowk cannot check, rather than half-checked.
//!
//! The roots are Sigstore's `trusted_root.json`, embedded as its TUF
//! repository publishes it (sigstore/root-signing, `targets/trusted_root.json`).
//! When Sigstore rotates a key, the file is replaced with the new one in a
//! release; a krowk built before a rotation cannot check a release signed
//! after it, says so, and points at the installer and SECURITY.md.

use base64::Engine;
use rustls_pki_types::{CertificateDer, UnixTime};
use serde_json::Value;
use sha2::{Digest, Sha256};

const TRUSTED_ROOT: &str = include_str!("trusted_root.json");
/// The workflow that signs releases; the SAN names it at the release's tag.
const REPOSITORY: &str = "https://github.com/krowkcom/krowk";
const WORKFLOW: &str = "https://github.com/krowkcom/krowk/.github/workflows/release.yml";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
/// What started the signing run: a tag pushed. A `workflow_dispatch` or
/// `workflow_call` added to release.yml later does not widen who may sign.
const TRIGGER: &str = "push";
/// id-kp-codeSigning (1.3.6.1.5.5.7.3.3), the EKU Fulcio's leaf and
/// intermediate both carry.
const CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];
/// Fulcio's extensions (1.3.6.1.4.1.57264.1.n): the OIDC issuer (8), the
/// source repository (12) and the build trigger (20), each a UTF8String, and
/// the older GitHub trigger (2), the bare string.
const OIDC_ISSUER: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x08];
const SOURCE_REPOSITORY: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x0c];
const BUILD_TRIGGER: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x14];
const GITHUB_TRIGGER_V1: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x02];

/// The identity a release's signature must carry: the release workflow at
/// that release's tag.
pub(super) fn identity(version: &str) -> String {
    format!("{WORKFLOW}@refs/tags/v{version}")
}

/// Checks `bundle` (a `.sigstore.json`) signs `message` as `identity`;
/// the reason it does not otherwise.
pub(super) fn verify(message: &[u8], bundle: &[u8], identity: &str) -> Result<(), String> {
    let root: Value = serde_json::from_str(TRUSTED_ROOT).map_err(|e| format!("the embedded Sigstore root is unreadable: {e}"))?;
    verify_with(&root, message, bundle, identity)
}

fn verify_with(root: &Value, message: &[u8], bundle: &[u8], identity: &str) -> Result<(), String> {
    let bundle: Value = serde_json::from_slice(bundle).map_err(|e| format!("the signature bundle is not JSON: {e}"))?;
    let media = str_at(&bundle, "/mediaType")?;
    if !matches!(media, "application/vnd.dev.sigstore.bundle.v0.3+json" | "application/vnd.dev.sigstore.bundle+json;version=0.3") {
        return Err(format!("the signature bundle is a kind this krowk does not read ({media})"));
    }
    let cert = b64(str_at(&bundle, "/verificationMaterial/certificate/rawBytes")?)?;
    let signature = b64(str_at(&bundle, "/messageSignature/signature")?)?;
    let digest = Sha256::digest(message);
    if str_at(&bundle, "/messageSignature/messageDigest/algorithm")? != "SHA2_256"
        || b64(str_at(&bundle, "/messageSignature/messageDigest/digest")?)? != digest[..]
    {
        return Err("the signature is for different bytes than these".into());
    }

    // The log first: it is what says when the certificate was used.
    let entries = bundle.pointer("/verificationMaterial/tlogEntries").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let [entry] = entries else {
        return Err(format!("the signature bundle carries {} transparency-log entries, not one", entries.len()));
    };
    let time = check_entry(root, entry, &digest, &signature, &cert)?;

    // The certificate: Fulcio's, at that time, for code signing, and naming
    // the release workflow at this tag, from GitHub's issuer.
    let cert_der = CertificateDer::from(cert.as_slice());
    let leaf = webpki::EndEntityCert::try_from(&cert_der).map_err(|e| format!("the signing certificate is unreadable: {e:?}"))?;
    let (anchors, intermediates) = fulcio(root, time)?;
    let anchors: Vec<_> = anchors.iter().map(|c| webpki::anchor_from_trusted_cert(c).map(|a| a.to_owned())).collect::<Result<_, _>>().map_err(|e| format!("the embedded Fulcio root is unreadable: {e:?}"))?;
    let algs = [webpki::ring::ECDSA_P384_SHA384, webpki::ring::ECDSA_P256_SHA256];
    let at = UnixTime::since_unix_epoch(std::time::Duration::from_secs(time));
    leaf.verify_for_usage(&algs, &anchors, &intermediates, at, webpki::KeyUsage::required(CODE_SIGNING), None, None)
        .map_err(|e| format!("the signing certificate is not one Sigstore's Fulcio issued: {e:?}"))?;
    check_signer(&Signer::of(&leaf, &cert), identity)?;
    leaf.verify_signature(webpki::ring::ECDSA_P256_SHA256, message, &signature)
        .map_err(|_| "the signature does not match these bytes".to_string())
}

/// Who a Fulcio certificate says signed: its SAN URIs and the extensions
/// that name the issuer, the repository and the trigger.
struct Signer<'a> {
    uris: Vec<&'a str>,
    issuer: Option<&'a [u8]>,
    repository: Option<&'a [u8]>,
    trigger: Option<&'a [u8]>,
    trigger_v1: Option<&'a [u8]>,
}

impl<'a> Signer<'a> {
    fn of(leaf: &webpki::EndEntityCert<'a>, cert: &'a [u8]) -> Self {
        let utf8 = |oid| extension(cert, oid).and_then(|v| der(v, 0x0c)).map(|(s, _)| s);
        Signer {
            uris: leaf.valid_uri_names().collect(),
            issuer: utf8(OIDC_ISSUER),
            repository: utf8(SOURCE_REPOSITORY),
            trigger: utf8(BUILD_TRIGGER),
            trigger_v1: extension(cert, GITHUB_TRIGGER_V1),
        }
    }
}

/// The release workflow at this tag, and nothing wider: one SAN, GitHub's
/// issuer, this repository, a push.
fn check_signer(s: &Signer, identity: &str) -> Result<(), String> {
    if s.uris != [identity] {
        return Err(format!("the signature is by {}, not by {identity}", s.uris.join(", ")));
    }
    if s.issuer != Some(ISSUER.as_bytes()) {
        return Err(format!("the signing identity was not issued by {ISSUER}"));
    }
    if s.repository != Some(REPOSITORY.as_bytes()) {
        return Err(format!("the signing run was not in {REPOSITORY}"));
    }
    if s.trigger != Some(TRIGGER.as_bytes()) || s.trigger_v1.is_some_and(|t| t != TRIGGER.as_bytes()) {
        return Err(format!("the signing run was not started by a {TRIGGER} of the tag"));
    }
    Ok(())
}

/// Checks the bundle's one Rekor entry: it logs this signature by this
/// certificate over this digest, Rekor promised to log it (the signed entry
/// timestamp), and the inclusion proof reaches a checkpoint Rekor signed.
/// The time it was logged, which the promise vouches for.
fn check_entry(root: &Value, entry: &Value, digest: &[u8], signature: &[u8], cert: &[u8]) -> Result<u64, String> {
    if str_at(entry, "/kindVersion/kind")? != "hashedrekord" || str_at(entry, "/kindVersion/version")? != "0.0.1" {
        return Err("the transparency-log entry is a kind this krowk cannot check (Rekor v1's hashedrekord 0.0.1 only): verify it by hand as SECURITY.md says".into());
    }
    let log_id = b64(str_at(entry, "/logId/keyId")?)?;
    let time: u64 = number(entry, "/integratedTime")?;
    let (key, origin) = rekor_key(root, &log_id, time)?;

    // What was logged is what is being checked.
    let body_b64 = str_at(entry, "/canonicalizedBody")?;
    let body_raw = b64(body_b64)?;
    let body: Value = serde_json::from_slice(&body_raw).map_err(|_| "the transparency-log entry is unreadable".to_string())?;
    let logged_cert = str_at(&body, "/spec/signature/publicKey/content").and_then(b64).and_then(|pem| pem_der(&pem))?;
    if str_at(&body, "/kind")? != "hashedrekord"
        || str_at(&body, "/spec/data/hash/algorithm")? != "sha256"
        || str_at(&body, "/spec/data/hash/value")? != hex(digest)
        || b64(str_at(&body, "/spec/signature/content")?)? != signature
        || logged_cert != cert
    {
        return Err("the transparency-log entry is for a different signature than the bundle's".into());
    }

    // The signed entry timestamp: Rekor's signature over the entry as it
    // logged it, in canonical JSON (keys sorted, no spaces).
    let index: u64 = number(entry, "/logIndex")?;
    let promise = b64(str_at(entry, "/inclusionPromise/signedEntryTimestamp").map_err(|_| "the transparency-log entry has no signed entry timestamp".to_string())?)?;
    let set = format!(r#"{{"body":"{body_b64}","integratedTime":{time},"logID":"{}","logIndex":{index}}}"#, hex(&log_id));
    webpki::ring::ECDSA_P256_SHA256.verify_signature(&key, set.as_bytes(), &promise).map_err(|_| "Rekor's signed entry timestamp does not verify".to_string())?;

    // The inclusion proof, up to the root hash its checkpoint carries, and
    // the checkpoint signed by the same key.
    let leaf = node(&[0], &body_raw, &[]);
    let proof_index: u64 = number(entry, "/inclusionProof/logIndex")?;
    let size: u64 = number(entry, "/inclusionProof/treeSize")?;
    let root_hash = b64(str_at(entry, "/inclusionProof/rootHash")?)?;
    let hashes = entry.pointer("/inclusionProof/hashes").and_then(Value::as_array).ok_or("the inclusion proof has no hashes")?;
    let hashes: Vec<Vec<u8>> = hashes.iter().map(|h| h.as_str().ok_or_else(|| "an inclusion proof hash is not a string".to_string()).and_then(b64)).collect::<Result<_, _>>()?;
    // SHA-256 throughout: anything else is no node of this tree.
    if root_hash.len() != 32 || hashes.iter().any(|h| h.len() != 32) {
        return Err("the inclusion proof holds a hash that is not 32 bytes".into());
    }
    if merkle_root(proof_index, size, leaf, &hashes).as_ref().map(|r| &r[..]) != Some(&root_hash[..]) {
        return Err("the inclusion proof does not reach its root".into());
    }
    let note = str_at(entry, "/inclusionProof/checkpoint/envelope")?;
    let (text, sigs) = note.split_once("\n\n").ok_or("the checkpoint is unreadable")?;
    let mut lines = text.lines();
    let (origin_line, size_line, root_line) = (lines.next(), lines.next(), lines.next());
    // The log's origin, `<host> - <tree id>`: a checkpoint of another log
    // the same key signed is not this one's.
    let tree = origin_line.and_then(|l| l.strip_prefix(origin.as_str())).and_then(|l| l.strip_prefix(" - "));
    if !tree.is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())) {
        return Err(format!("the checkpoint is not {origin}'s"));
    }
    if size_line != Some(&size.to_string()) || root_line.and_then(|l| b64(l).ok()).as_deref() != Some(&root_hash[..]) {
        return Err("the checkpoint is for a different tree than the inclusion proof".into());
    }
    let signed = format!("{text}\n");
    let ok = sigs.lines().filter_map(|l| l.strip_prefix("\u{2014} ")).filter_map(|l| l.split_once(' ')).any(|(_, sig)| {
        b64(sig).is_ok_and(|s| s.len() > 4 && s[..4] == log_id[..4] && webpki::ring::ECDSA_P256_SHA256.verify_signature(&key, signed.as_bytes(), &s[4..]).is_ok())
    });
    if !ok {
        return Err("Rekor's checkpoint signature does not verify".into());
    }
    Ok(time)
}

/// Rekor's key with that log id, an ECDSA P-256 one valid at `time`: its
/// raw public point, and the log's host, which its checkpoints name.
fn rekor_key(root: &Value, log_id: &[u8], time: u64) -> Result<(Vec<u8>, String), String> {
    for log in root.get("tlogs").and_then(Value::as_array).into_iter().flatten() {
        let Ok(id) = str_at(log, "/logId/keyId").and_then(b64) else { continue };
        if id != log_id || str_at(log, "/publicKey/keyDetails") != Ok("PKIX_ECDSA_P256_SHA_256") || !valid_at(log.pointer("/publicKey/validFor"), time) {
            continue;
        }
        let spki = b64(str_at(log, "/publicKey/rawBytes")?)?;
        // SubjectPublicKeyInfo: the algorithm, then the key as a BIT STRING
        // with no unused bits.
        let key = der(&spki, 0x30).and_then(|(spki, _)| der(spki, 0x30).and_then(|(_, rest)| der(rest, 0x03))).map(|(k, _)| k);
        let origin = str_at(log, "/baseUrl")?.strip_prefix("https://").unwrap_or_default().trim_end_matches('/').to_string();
        return match key {
            Some([0, point @ ..]) if !origin.is_empty() => Ok((point.to_vec(), origin)),
            _ => Err("the embedded Rekor key is unreadable".into()),
        };
    }
    Err("the signature was logged in a transparency log this krowk does not know: verify it by hand as SECURITY.md says".into())
}

/// Fulcio's roots and intermediates valid at `time`, from every certificate
/// authority the trusted root lists.
type Chains = (Vec<CertificateDer<'static>>, Vec<CertificateDer<'static>>);
fn fulcio(root: &Value, time: u64) -> Result<Chains, String> {
    let (mut anchors, mut intermediates) = (Vec::new(), Vec::new());
    for ca in root.get("certificateAuthorities").and_then(Value::as_array).into_iter().flatten() {
        if !valid_at(ca.get("validFor"), time) {
            continue;
        }
        let chain = ca.pointer("/certChain/certificates").and_then(Value::as_array).into_iter().flatten();
        let mut chain: Vec<CertificateDer<'static>> = chain.map(|c| str_at(c, "/rawBytes").and_then(b64).map(CertificateDer::from)).collect::<Result<_, _>>()?;
        // Leaf-first, as the trusted root lists it: the last is the root.
        if let Some(anchor) = chain.pop() {
            anchors.push(anchor);
            intermediates.extend(chain);
        }
    }
    if anchors.is_empty() {
        return Err("no Fulcio root this krowk knows was valid when the release was signed: verify it by hand as SECURITY.md says".into());
    }
    Ok((anchors, intermediates))
}

/// A trusted root's `validFor`: from `start`, until `end` when it has one.
fn valid_at(range: Option<&Value>, time: u64) -> bool {
    let at = |k: &str| range.and_then(|r| r.get(k)).and_then(Value::as_str).and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(|t| t.as_second());
    let Ok(time) = i64::try_from(time) else { return false };
    at("start").is_some_and(|s| s <= time) && at("end").is_none_or(|e| time <= e)
}

/// RFC 9162's inclusion proof (2.1.3.2): the root `leaf` at `index` reaches
/// in a tree of `size` leaves through `proof`.
fn merkle_root(index: u64, size: u64, leaf: [u8; 32], proof: &[Vec<u8>]) -> Option<[u8; 32]> {
    if index >= size {
        return None;
    }
    let (mut f, mut s, mut r) = (index, size - 1, leaf);
    for p in proof {
        if s == 0 {
            return None;
        }
        if f & 1 == 1 || f == s {
            r = node(&[1], p, &r);
            while f & 1 == 0 && f != 0 {
                f >>= 1;
                s >>= 1;
            }
        } else {
            r = node(&[1], &r, p);
        }
        f >>= 1;
        s >>= 1;
    }
    (s == 0).then_some(r)
}

fn node(prefix: &[u8], a: &[u8], b: &[u8]) -> [u8; 32] {
    Sha256::new().chain_update(prefix).chain_update(a).chain_update(b).finalize().into()
}

/// The value of the extension `oid` in a certificate's DER.
fn extension<'a>(cert: &'a [u8], oid: &[u8]) -> Option<&'a [u8]> {
    let (cert, _) = der(cert, 0x30)?;
    let (mut tbs, _) = der(cert, 0x30)?;
    // The TBSCertificate's fields up to [3], the extensions.
    let exts = loop {
        let (tag, value, rest) = tlv(tbs)?;
        if tag == 0xa3 {
            break der(value, 0x30)?.0;
        }
        tbs = rest;
    };
    let mut exts = exts;
    while !exts.is_empty() {
        let (ext, rest) = der(exts, 0x30)?;
        exts = rest;
        let (id, mut ext) = der(ext, 0x06)?;
        if id != oid {
            continue;
        }
        if let Some((_, rest)) = der(ext, 0x01) {
            ext = rest;
        }
        return der(ext, 0x04).map(|(v, _)| v);
    }
    None
}

/// The next DER element, if it has tag `want`: its contents and what follows.
fn der(d: &[u8], want: u8) -> Option<(&[u8], &[u8])> {
    tlv(d).filter(|(tag, ..)| *tag == want).map(|(_, v, rest)| (v, rest))
}

/// The next DER element: its tag, its contents and what follows it.
fn tlv(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, d) = d.split_first()?;
    let (&first, d) = d.split_first()?;
    let (len, d) = if first < 0x80 {
        (usize::from(first), d)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || d.len() < n {
            return None;
        }
        (d[..n].iter().fold(0usize, |a, &b| a << 8 | usize::from(b)), &d[n..])
    };
    (d.len() >= len).then(|| (tag, &d[..len], &d[len..]))
}

fn str_at<'a>(v: &'a Value, path: &str) -> Result<&'a str, String> {
    v.pointer(path).and_then(Value::as_str).ok_or_else(|| format!("the signature bundle has no {path}"))
}

/// A protobuf-JSON integer, which arrives as a string.
fn number(v: &Value, path: &str) -> Result<u64, String> {
    str_at(v, path)?.parse().map_err(|_| format!("the signature bundle's {path} is not a number"))
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| "the signature bundle holds base64 that does not decode".to_string())
}

fn pem_der(pem: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "the logged certificate is not PEM".to_string())?;
    b64(&text.lines().filter(|l| !l.starts_with("-----")).collect::<String>())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &[u8] = include_bytes!("../../../tests/fixtures/release-v0.13.0/checksums.txt");
    const BUNDLE: &str = include_str!("../../../tests/fixtures/release-v0.13.0/checksums.txt.sigstore.json");

    /// The recorded bundle with one field replaced.
    fn tampered(path: &str, to: Value) -> Vec<u8> {
        let mut b: Value = serde_json::from_str(BUNDLE).unwrap();
        *b.pointer_mut(path).unwrap() = to;
        b.to_string().into_bytes()
    }

    #[test]
    fn r_oss_3_a_real_release_signature_verifies_as_the_release_workflow_at_its_tag() {
        assert_eq!(verify(SUMS, BUNDLE.as_bytes(), &identity("0.13.0")), Ok(()));
    }

    #[test]
    fn r_oss_3_a_checksum_changed_after_signing_is_refused() {
        let mut sums = SUMS.to_vec();
        sums[0] = if sums[0] == b'0' { b'1' } else { b'0' };
        let err = verify(&sums, BUNDLE.as_bytes(), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("different bytes"), "{err}");
    }

    #[test]
    fn r_oss_3_a_signature_for_another_tag_or_workflow_is_refused() {
        let err = verify(SUMS, BUNDLE.as_bytes(), &identity("0.13.1")).unwrap_err();
        assert!(err.contains("not by https://github.com/krowkcom/krowk/.github/workflows/release.yml@refs/tags/v0.13.1"), "{err}");
        let err = verify(SUMS, BUNDLE.as_bytes(), "https://github.com/someone/krowk/.github/workflows/release.yml@refs/tags/v0.13.0").unwrap_err();
        assert!(err.contains("not by"), "{err}");
    }

    #[test]
    fn r_oss_3_a_signature_that_is_not_the_certificates_is_refused() {
        // A valid DER ECDSA signature from somewhere else: Rekor's own, over
        // the entry. The digest still matches, so only the key check stops it.
        let b: Value = serde_json::from_str(BUNDLE).unwrap();
        let other = b.pointer("/verificationMaterial/tlogEntries/0/inclusionPromise/signedEntryTimestamp").unwrap().clone();
        let err = verify(SUMS, &tampered("/messageSignature/signature", other), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("different signature"), "the log holds the real one: {err}");
    }

    #[test]
    fn r_oss_3_a_certificate_fulcio_did_not_issue_is_refused() {
        // Rekor's entry is checked against the bundle's certificate, so a
        // swapped certificate fails there; with no Fulcio root it fails the
        // chain on its own.
        let root: Value = serde_json::from_str(TRUSTED_ROOT).unwrap();
        let mut no_fulcio = root.clone();
        no_fulcio["certificateAuthorities"] = serde_json::json!([]);
        let err = verify_with(&no_fulcio, SUMS, BUNDLE.as_bytes(), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("no Fulcio root"), "{err}");
        let mut other_root = root.clone();
        let leaf = bundle_cert();
        other_root["certificateAuthorities"][1]["certChain"]["certificates"] = serde_json::json!([{ "rawBytes": leaf }]);
        let err = verify_with(&other_root, SUMS, BUNDLE.as_bytes(), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("not one Sigstore's Fulcio issued"), "{err}");
    }

    fn bundle_cert() -> String {
        let b: Value = serde_json::from_str(BUNDLE).unwrap();
        b.pointer("/verificationMaterial/certificate/rawBytes").unwrap().as_str().unwrap().to_string()
    }

    #[test]
    fn r_oss_3_the_transparency_log_entry_is_checked_not_just_carried() {
        let path = "/verificationMaterial/tlogEntries/0";
        let b: Value = serde_json::from_str(BUNDLE).unwrap();
        let entry = b.pointer(path).unwrap();
        // A forged time, to bring the ten-minute certificate back into its
        // validity: the signed entry timestamp no longer verifies.
        let err = verify(SUMS, &tampered(&format!("{path}/integratedTime"), "1791389999".into()), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("signed entry timestamp"), "{err}");
        let mut hashes = entry.pointer("/inclusionProof/hashes").unwrap().clone();
        hashes[0] = hashes[1].clone();
        let err = verify(SUMS, &tampered(&format!("{path}/inclusionProof/hashes"), hashes), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("inclusion proof"), "{err}");
        let note = entry.pointer("/inclusionProof/checkpoint/envelope").unwrap().as_str().unwrap();
        let forged = note.replacen("wNI9ajBEAiAXlFise4", "wNI9ajBEAiAXlFise5", 1);
        let err = verify(SUMS, &tampered(&format!("{path}/inclusionProof/checkpoint/envelope"), forged.into()), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("checkpoint signature"), "{err}");
        for origin in ["rekor.sigstore.dev  - 1193050959916656506", "log2025-1.rekor.sigstore.dev - 1", "rekor.sigstore.dev - ", "rekor.sigstore.dev - 11x"] {
            let forged = note.replacen("rekor.sigstore.dev - 1193050959916656506", origin, 1);
            let err = verify(SUMS, &tampered(&format!("{path}/inclusionProof/checkpoint/envelope"), forged.into()), &identity("0.13.0")).unwrap_err();
            assert!(err.contains("not rekor.sigstore.dev's"), "{origin}: {err}");
        }
        let mut short = entry.pointer("/inclusionProof/hashes").unwrap().clone();
        short[0] = base64::engine::general_purpose::STANDARD.encode([0u8; 31]).into();
        let err = verify(SUMS, &tampered(&format!("{path}/inclusionProof/hashes"), short), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("not 32 bytes"), "{err}");
        let err = verify(SUMS, &tampered(&format!("{path}/kindVersion/version"), "0.0.2".into()), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("by hand"), "a kind it cannot check is refused, and the way round named: {err}");
        let err = verify(SUMS, &tampered("/verificationMaterial/tlogEntries", serde_json::json!([])), &identity("0.13.0")).unwrap_err();
        assert!(err.contains("0 transparency-log entries"), "{err}");
    }

    #[test]
    fn r_oss_3_the_signer_is_this_repository_on_a_push_and_nothing_wider() {
        let cert = b64(&bundle_cert()).unwrap();
        let der = CertificateDer::from(cert.as_slice());
        let leaf = webpki::EndEntityCert::try_from(&der).unwrap();
        let id = identity("0.13.0");
        assert_eq!(check_signer(&Signer::of(&leaf, &cert), &id), Ok(()), "the recorded release's certificate names all of it");
        let refused = |change: &dyn Fn(&mut Signer), says: &str| {
            let mut s = Signer::of(&leaf, &cert);
            change(&mut s);
            let err = check_signer(&s, &id).unwrap_err();
            assert!(err.contains(says), "{says}: {err}");
        };
        refused(&|s| s.uris.push("https://example.com/other"), "not by");
        refused(&|s| s.issuer = Some(b"https://gitlab.com"), "not issued by");
        refused(&|s| s.repository = Some(b"https://github.com/someone/krowk"), "not in https://github.com/krowkcom/krowk");
        refused(&|s| s.repository = None, "not in");
        refused(&|s| s.trigger = Some(b"workflow_dispatch"), "push");
        refused(&|s| s.trigger = None, "push");
        refused(&|s| s.trigger_v1 = Some(b"workflow_call"), "push");
    }

    /// The release workflow runs this on the bundle it has just signed,
    /// before anything is published (release.yml: "Check the signature as
    /// krowk upgrade will"): a change in what cosign writes fails the
    /// release, not every krowk's next upgrade.
    #[test]
    #[ignore = "needs KROWK_RELEASE_DIR and KROWK_RELEASE_VERSION, which the release workflow sets"]
    fn r_oss_3_the_release_being_published_verifies_as_krowk_upgrade_will_check_it() {
        let dir = std::path::PathBuf::from(std::env::var("KROWK_RELEASE_DIR").expect("KROWK_RELEASE_DIR"));
        let version = std::env::var("KROWK_RELEASE_VERSION").expect("KROWK_RELEASE_VERSION");
        let sums = std::fs::read(dir.join("checksums.txt")).unwrap();
        let bundle = std::fs::read(dir.join("checksums.txt.sigstore.json")).unwrap();
        assert_eq!(verify(&sums, &bundle, &identity(&version)), Ok(()));
    }

    #[test]
    fn the_inclusion_proof_follows_rfc_9162() {
        // A tree of three leaves: root = H(1, H(1, l0, l1), l2).
        let l: Vec<[u8; 32]> = (0u8..3).map(|i| node(&[0], &[i], &[])).collect();
        let n01 = node(&[1], &l[0], &l[1]);
        let root = node(&[1], &n01, &l[2]);
        assert_eq!(merkle_root(0, 3, l[0], &[l[1].to_vec(), l[2].to_vec()]), Some(root));
        assert_eq!(merkle_root(2, 3, l[2], &[n01.to_vec()]), Some(root));
        assert_ne!(merkle_root(1, 3, l[1], &[l[0].to_vec()]), Some(root), "a short proof");
        assert_eq!(merkle_root(3, 3, l[2], &[n01.to_vec()]), None, "an index past the tree");
    }

    #[test]
    fn the_embedded_root_names_the_public_fulcio_and_rekor() {
        let root: Value = serde_json::from_str(TRUSTED_ROOT).unwrap();
        let rekor = b64("wNI9atQGlz+VWfO6LRygH4QUfY/8W4RFwiT5i5WRgB0=").unwrap();
        assert_eq!(rekor_key(&root, &rekor, 1_791_389_941).map(|(k, o)| (k.len(), o)), Ok((65, "rekor.sigstore.dev".into())), "an uncompressed P-256 point");
        assert!(fulcio(&root, 1_791_389_941).is_ok_and(|(a, i)| a.len() == 1 && i.len() == 1));
    }
}
