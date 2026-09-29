//! A relay ticket (canon, engineering/relay.md → Tickets): the registry's
//! signed word that a device may join a session's relay channel, in a role,
//! for five minutes. The relay checks it locally, with the registry's public
//! key, so a join costs the registry nothing and a stranger costs the relay
//! nothing but a signature check.
//!
//! One fixed layout, the same in the registry (Ruby), the hosted relay (JS),
//! the reference relay and the stand-in registry (this module):
//!
//! | off | len | field |
//! |---|---|---|
//! | 0 | 1 | version, 1 |
//! | 1 | 8 | kid, the registry key that signed it |
//! | 9 | 1 | role: 1 host, 2 viewer |
//! | 10 | 1 | env: 1 production, 2 development |
//! | 11 | 16 | session id |
//! | 27 | 16 | device id |
//! | 43 | 32 | the device's signing public key |
//! | 75 | 8 | fence, big-endian (0 for a viewer) |
//! | 83 | 8 | issued at, unix seconds |
//! | 91 | 8 | expires at, unix seconds |
//! | 99 | 1 | n, the workspace's length, 1 to 64 |
//! | 100 | n | workspace, ASCII letters, digits, `_` and `-` |
//!
//! then a 64-byte Ed25519 signature over `krowk/relay-ticket/v1` and those
//! bytes. On the wire it is hex.

use crate::e2e::{self, RELAY_ROLE_HOST, RELAY_ROLE_VIEWER};
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};

pub const LABEL: &[u8] = b"krowk/relay-ticket/v1";
pub const VERSION: u8 = 1;
/// How long a ticket the registry issues lasts, and the longest a relay
/// accepts: a ticket's lifetime is the revocation lag, so it is short.
pub const TTL: u64 = 300;
pub const MAX_LIFETIME: u64 = TTL;
/// How far ahead of the relay's clock an issue time may be.
pub const SKEW: u64 = 60;
/// How long a channel nobody is on keeps its facts, its fence among them,
/// derived rather than chosen: a relay forgets a channel's fence only once
/// every ticket issued before the lease last moved has expired, which is at
/// most a lifetime and a skew after the last join (relay.md → Tickets).
pub const IDLE_MARGIN: u64 = 60;
pub const IDLE_FORGET: u64 = MAX_LIFETIME + SKEW + IDLE_MARGIN;
const _: () = assert!(IDLE_FORGET >= MAX_LIFETIME + SKEW + IDLE_MARGIN && IDLE_FORGET == 420);
pub const ENV_PRODUCTION: u8 = 1;
pub const ENV_DEVELOPMENT: u8 = 2;
const MAX_JSON_INT: u64 = (1 << 53) - 1;
const FIXED: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub kid: [u8; 8],
    pub role: u8,
    pub env: u8,
    pub session: [u8; 16],
    pub device: [u8; 16],
    pub signing_key: [u8; 32],
    pub fence: u64,
    pub iat: u64,
    pub exp: u64,
    pub workspace: String,
}

fn workspace_ok(w: &str) -> bool {
    (1..=64).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Ticket {
    fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(FIXED + self.workspace.len());
        b.push(VERSION);
        b.extend_from_slice(&self.kid);
        b.push(self.role);
        b.push(self.env);
        b.extend_from_slice(&self.session);
        b.extend_from_slice(&self.device);
        b.extend_from_slice(&self.signing_key);
        b.extend_from_slice(&self.fence.to_be_bytes());
        b.extend_from_slice(&self.iat.to_be_bytes());
        b.extend_from_slice(&self.exp.to_be_bytes());
        b.push(self.workspace.len() as u8);
        b.extend_from_slice(self.workspace.as_bytes());
        b
    }

    /// Signed with the issuer's 32-byte seed, as hex for the wire. For the
    /// stand-in registry and the conformance suite; the registry signs its
    /// own in Ruby, to the same bytes.
    pub fn sign(&self, seed: &[u8; 32]) -> String {
        let body = self.body();
        let sig = SigningKey::from_bytes(seed).sign(&[LABEL, &body].concat());
        e2e::hex(&[&body[..], &sig.to_bytes()].concat())
    }
}

/// Why a ticket was refused: `bad_ticket` or `ticket_expired`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: &'static str,
    pub message: String,
}

fn bad(m: impl Into<String>) -> Refused {
    Refused { code: "bad_ticket", message: m.into() }
}

/// Reads and checks a ticket: its layout strictly, its signature under the
/// key its kid names in `keys` (strictly, as a join's is), and its
/// lifetime against `now`. Nothing about what it is presented for: that is
/// the relay's to compare.
pub fn verify(hex: &str, keys: &[([u8; 8], [u8; 32])], now: u64) -> Result<Ticket, Refused> {
    verify_within(hex, keys, now, MAX_LIFETIME, SKEW)
}

/// `verify` with its lifetime cap and skew given: the production values,
/// or shortened ones for a test of the idle-forget invariant.
pub fn verify_within(hex: &str, keys: &[([u8; 8], [u8; 32])], now: u64, max_lifetime: u64, skew: u64) -> Result<Ticket, Refused> {
    let raw = (hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| e2e::unhex(&hex.to_ascii_lowercase())).flatten().ok_or_else(|| bad("the ticket is not hex"))?;
    if raw.len() < FIXED + 1 + 64 {
        return Err(bad("the ticket is shorter than its layout"));
    }
    let n = raw[99] as usize;
    if raw.len() != FIXED + n + 64 {
        return Err(bad("the ticket's length is not its layout's"));
    }
    let (body, sig) = raw.split_at(FIXED + n);
    let u64_at = |at: usize| u64::from_be_bytes(body[at..at + 8].try_into().expect("eight bytes"));
    let t = Ticket {
        kid: body[1..9].try_into().expect("8"),
        role: body[9],
        env: body[10],
        session: body[11..27].try_into().expect("16"),
        device: body[27..43].try_into().expect("16"),
        signing_key: body[43..75].try_into().expect("32"),
        fence: u64_at(75),
        iat: u64_at(83),
        exp: u64_at(91),
        workspace: String::from_utf8(body[FIXED..].to_vec()).map_err(|_| bad("the ticket's workspace is not text"))?,
    };
    if body[0] != VERSION {
        return Err(bad(format!("ticket version {} is not {VERSION}", body[0])));
    }
    if ![RELAY_ROLE_HOST, RELAY_ROLE_VIEWER].contains(&t.role) || ![ENV_PRODUCTION, ENV_DEVELOPMENT].contains(&t.env) {
        return Err(bad("the ticket names no role or env the relay knows"));
    }
    if (t.role == RELAY_ROLE_VIEWER && t.fence != 0) || t.fence > MAX_JSON_INT || t.iat > MAX_JSON_INT || t.exp > MAX_JSON_INT || t.exp <= t.iat || t.exp - t.iat > max_lifetime || !workspace_ok(&t.workspace) {
        return Err(bad("the ticket's fields are out of range"));
    }
    let key = keys.iter().find(|(kid, _)| *kid == t.kid).map(|(_, k)| k).ok_or_else(|| bad(format!("no registry key {} signs tickets here", e2e::hex(&t.kid))))?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| bad("the registry key is not an Ed25519 key"))?;
    let sig = ed25519_dalek::Signature::from_bytes(sig.try_into().expect("64"));
    key.verify_strict(&[LABEL, body].concat(), &sig).map_err(|_| bad("the ticket's signature is not the registry's"))?;
    // Not yet valid, or expired; and never alive longer from now than a
    // lifetime and the skew, whatever the ticket's own times say.
    if now >= t.exp || t.iat > now + skew || t.exp > now + max_lifetime + skew {
        return Err(Refused { code: "ticket_expired", message: "the ticket has expired, or is not yet valid".into() });
    }
    Ok(t)
}

/// One of the registry's ticket keys, `"<16 hex kid>": "<64 hex key>"`,
/// read strictly.
pub fn parse_key(kid: &str, key: &str) -> Result<([u8; 8], [u8; 32]), String> {
    let k: [u8; 8] = e2e::unhex(kid).and_then(|b| b.try_into().ok()).ok_or_else(|| format!("ticket kid {kid:?} is not 16 hex characters"))?;
    let p: [u8; 32] = e2e::unhex(key).and_then(|b| b.try_into().ok()).ok_or_else(|| format!("ticket key {kid} is not 64 hex characters"))?;
    Ok((k, p))
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        Ticket { kid: [1, 2, 3, 4, 5, 6, 7, 8], role: RELAY_ROLE_HOST, env: ENV_PRODUCTION, session: [9; 16], device: [7; 16], signing_key: [5; 32], fence: 4, iat: 1_000, exp: 1_300, workspace: "ws_test".into() }
    }

    fn keys() -> Vec<([u8; 8], [u8; 32])> {
        vec![([1, 2, 3, 4, 5, 6, 7, 8], SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes())]
    }

    /// R-RELAY-1: a ticket verifies under its kid's key, within its life,
    /// and every other ticket is refused.
    #[test]
    fn r_relay_1_a_ticket_verifies_and_every_altered_one_is_refused() {
        let wire = ticket().sign(&[1; 32]);
        assert_eq!(verify(&wire, &keys(), 1_100).unwrap(), ticket());
        assert_eq!(verify(&wire, &keys(), 1_300).unwrap_err().code, "ticket_expired");
        assert_eq!(verify(&ticket().sign(&[2; 32]), &keys(), 1_100).unwrap_err().code, "bad_ticket", "another key under the right kid");
        assert_eq!(verify(&Ticket { kid: [0; 8], ..ticket() }.sign(&[1; 32]), &keys(), 1_100).unwrap_err().code, "bad_ticket", "an unknown kid");
        assert_eq!(verify(&wire[..wire.len() - 2], &keys(), 1_100).unwrap_err().code, "bad_ticket", "truncated");
        let mut flipped = e2e::unhex(&wire).unwrap();
        flipped[80] ^= 1;
        assert_eq!(verify(&e2e::hex(&flipped), &keys(), 1_100).unwrap_err().code, "bad_ticket", "a field changed after signing");
        for t in [Ticket { role: 3, ..ticket() }, Ticket { env: 0, ..ticket() }, Ticket { role: RELAY_ROLE_VIEWER, ..ticket() }, Ticket { exp: 1_301, ..ticket() }, Ticket { workspace: "ws a".into(), ..ticket() }, Ticket { fence: 1 << 53, ..ticket() }] {
            assert_eq!(verify(&t.sign(&[1; 32]), &keys(), 1_100).unwrap_err().code, "bad_ticket", "{t:?}");
        }
    }

    /// The registry's own ticket, from the golden both repos carry
    /// (`tests/fixtures/relay_ticket.golden`, the registry's
    /// `test/fixtures/relay_ticket.golden`), whose SHA-256 relay.md records:
    /// it reads and verifies here byte for byte, and this module signs the
    /// same bytes. A golden regenerated on one side fails the hash here.
    #[test]
    fn r_relay_1_the_registrys_ticket_is_this_layout() {
        use sha2::{Digest, Sha256};
        const GOLDEN: &str = include_str!("../tests/fixtures/relay_ticket.golden");
        assert_eq!(e2e::hex(&Sha256::digest(GOLDEN.as_bytes())), "0676411b37dc8cf591afacfb4f8df02462f9e6a10042ef71ac0c59f9383d5b55", "relay.md's recorded golden");
        let field = |name: &str| {
            let at = GOLDEN.find(&format!("\"{name}\"")).unwrap_or_else(|| panic!("the golden names {name}"));
            let rest = GOLDEN[at..].split_once(':').unwrap().1.trim_start();
            rest.trim_start_matches('"').split(['"', ',', '\n']).next().unwrap().trim().to_string()
        };
        let wire = field("ticket");
        let seed: [u8; 32] = e2e::unhex(&field("seed")).unwrap().try_into().unwrap();
        let kid: [u8; 8] = e2e::unhex(&field("kid")).unwrap().try_into().unwrap();
        let keys = vec![(kid, SigningKey::from_bytes(&seed).verifying_key().to_bytes())];
        let iat: u64 = field("iat").parse().unwrap();
        let t = verify(&wire, &keys, iat + 100).unwrap();
        assert_eq!((t.role, t.env, t.fence, t.workspace.clone()), (RELAY_ROLE_HOST, ENV_PRODUCTION, field("fence").parse().unwrap(), field("workspace")));
        assert_eq!(e2e::hex(&t.device), field("device"));
        assert_eq!(t.sign(&seed), wire);
    }
}
