//! Joining a session's channel on the relay (engineering/relay.md →
//! Joining), with no IO and no clock: where to dial, and a state machine
//! that is handed each binary WebSocket message received and answers what
//! to send. Every client links it (R-CLIENT-1): the harness drives it over
//! tokio-tungstenite, the phone over Network.framework, and neither
//! re-derives the signature's layout. Opening the socket, TLS and the
//! 10-second waits are the caller's.
//!
//! The challenge's own `relay` field is never read: the origin signed is
//! the one dialed, reduced to its canonical form, so a relay cannot pass
//! another relay's challenge through and join there as this device.

use crate::e2e::{self, DeviceId, SigningKey};
use crate::protocol::frame::{HEADER, KIND_RELAY, V};
use serde_json::{json, Value};

/// Where to dial and what to send with the upgrade: `ticket` goes in its
/// `X-Krowk-Ticket` header, never in the URL, so no log of URLs holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dial {
    pub url: String,
    pub ticket: String,
}

/// `{relay}/v1/relay/{session}`, with `?env={env}` unless `env` is
/// `production`.
pub fn dial(relay: &str, session: &str, env: &str, ticket: &str) -> Dial {
    let base = relay.trim_end_matches('/');
    let query = if env == "production" { String::new() } else { format!("?env={env}") };
    Dial { url: format!("{base}/v1/relay/{session}{query}"), ticket: ticket.to_string() }
}

/// One join in progress, from the dial to `joined`.
pub struct Joining {
    role: u8,
    session: [u8; 16],
    env: String,
    device: DeviceId,
    /// The canonical origin of the relay as dialed: what is signed.
    origin: String,
    extra: Value,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitChallenge,
    AwaitJoined,
    Done,
}

/// What a message received comes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Nothing to do yet: read the next message.
    Wait,
    /// Send these bytes as one binary message, then read the next.
    Send(Vec<u8>),
    /// On the channel: the relay's `joined`.
    Joined(Value),
}

impl Joining {
    /// `relay` as dialed: its canonical origin is what gets signed, and a
    /// relay URL with none is refused here, before anything is sent.
    /// `extra` is merged into the join: a host's `fence` and `stream`, a
    /// viewer's `stream` and `afterSeq`. They are not signed, on purpose.
    pub fn new(relay: &str, session: &str, env: &str, device: DeviceId, role: u8, extra: Value) -> Result<Joining, String> {
        let base = relay.trim_end_matches('/');
        let origin = e2e::canonical_origin(base).ok_or_else(|| format!("{base} has no origin a relay signs"))?;
        Ok(Joining { role, session: super::uuid(session), env: env.to_string(), device, origin, extra, state: State::AwaitChallenge })
    }

    /// One binary WebSocket message received. Anything but a relay control
    /// message is `Wait`. The challenge answers the join to send; `joined`
    /// ends it; anything else in their place is the relay's refusal. Once
    /// it has ended — joined, or any error — every message is an error, so
    /// no second challenge is ever signed.
    pub fn message(&mut self, signing: &SigningKey, bytes: &[u8]) -> Result<Step, String> {
        // Done unless this message moves it on: whatever goes wrong ends
        // the join, and one challenge is answered, once.
        let state = std::mem::replace(&mut self.state, State::Done);
        if state == State::Done {
            return Err("the join is over".into());
        }
        if !is_control(bytes) {
            self.state = state;
            return Ok(Step::Wait);
        }
        // Serde's own words, as the harness has always said them.
        let v: Value = serde_json::from_slice(&bytes[HEADER..]).map_err(|e| e.to_string())?;
        if state == State::AwaitJoined {
            if v["type"] != "joined" {
                return Err(refusal(&v));
            }
            return Ok(Step::Joined(v));
        }
        if v["type"] != "challenge" {
            return Err(refusal(&v));
        }
        let nonce: [u8; 32] = v["nonce"].as_str().and_then(e2e::unhex).and_then(|n| n.try_into().ok()).ok_or("the relay's challenge has no nonce")?;
        let sig = signing.sign_relay_join(self.role, &self.session, &nonce, &self.device, &self.origin).map_err(|e| e.to_string())?;
        let mut msg = json!({"type": "join", "role": if self.role == e2e::RELAY_ROLE_HOST { "host" } else { "viewer" }, "device": self.device.to_string(), "signature": e2e::hex(&sig), "env": self.env});
        if let Value::Object(extra) = std::mem::take(&mut self.extra) {
            for (k, v) in extra {
                msg[k] = v;
            }
        }
        self.state = State::AwaitJoined;
        Ok(Step::Send(control(&msg)))
    }
}

fn is_control(b: &[u8]) -> bool {
    b.len() >= HEADER && b[1] == KIND_RELAY
}

/// A relay control message (kind 4) as sent: the 28-byte header with no
/// session and no seq, then the JSON.
pub fn control(v: &Value) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER + 64);
    b.extend_from_slice(&[V, KIND_RELAY, 0, 0]);
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(&0u64.to_be_bytes());
    serde_json::to_writer(&mut b, v).expect("a JSON value serializes");
    b
}

/// A relay control message as read: its JSON, or none for any other
/// message (or one whose payload is not JSON).
pub fn read_control(bytes: &[u8]) -> Option<Value> {
    if !is_control(bytes) {
        return None;
    }
    serde_json::from_slice(&bytes[HEADER..]).ok()
}

/// What a relay's `error` (or anything else where it was not expected)
/// says, as the person reads it.
pub fn refusal(v: &Value) -> String {
    format!("the relay refused: {} — {}", v["code"].as_str().unwrap_or("?"), v["message"].as_str().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{RELAY_ROLE_HOST, RELAY_ROLE_VIEWER};

    const SESSION: &str = "01a0f731-f1c0-7000-92d7-64b247e51781";
    const NONCE: [u8; 32] = [9; 32];

    fn signing() -> SigningKey {
        SigningKey::from_secret(&[7; 32]).unwrap()
    }

    fn challenge() -> Vec<u8> {
        control(&json!({"type": "challenge", "version": 1, "nonce": e2e::hex(&NONCE), "relay": "wss://elsewhere.example", "session": "01a0f731f1c0700092d764b247e51781"}))
    }

    /// The join a challenge is answered with, read back from its bytes.
    fn join_sent(relay: &str, role: u8, extra: Value) -> (Joining, Value) {
        let mut j = Joining::new(relay, SESSION, "development", DeviceId([1; 16]), role, extra).unwrap();
        let Step::Send(b) = j.message(&signing(), &challenge()).unwrap() else { panic!("the challenge is answered") };
        assert_eq!(b[..4], [V, KIND_RELAY, 0, 0], "a kind-4 envelope");
        assert_eq!(b[4..HEADER], [0; 24], "no session and no seq");
        (j, read_control(&b).expect("the join is a control message"))
    }

    fn verifies(v: &Value, role: u8, origin: &str) -> bool {
        let sig = e2e::unhex(v["signature"].as_str().unwrap()).unwrap();
        signing().public().verify_relay_join(&sig, role, &crate::sync::uuid(SESSION), &NONCE, &DeviceId([1; 16]), origin).is_ok()
    }

    /// R-RELAY-1, R-CLIENT-1: the challenge is answered with a kind-4 join
    /// holding every field the relay reads and the caller's `extra`, signed
    /// over the canonical origin dialed — not the one the challenge names.
    #[test]
    fn r_relay_1_a_challenge_is_answered_with_a_join_signed_over_the_origin_dialed() {
        let (mut j, v) = join_sent("ws://127.0.0.1:7790/", RELAY_ROLE_HOST, json!({"fence": 3, "stream": "ab".repeat(16)}));
        assert_eq!(v["type"], "join");
        assert_eq!(v["role"], "host");
        assert_eq!(v["device"], "01".repeat(16));
        assert_eq!(v["env"], "development");
        assert_eq!(v["fence"], 3);
        assert_eq!(v["stream"], "ab".repeat(16));
        assert_eq!(v["signature"].as_str().unwrap().len(), 128);
        assert!(verifies(&v, RELAY_ROLE_HOST, "ws://127.0.0.1:7790"));
        assert!(!verifies(&v, RELAY_ROLE_HOST, "wss://elsewhere.example"), "the challenge's relay is never signed");
        assert!(!verifies(&v, RELAY_ROLE_VIEWER, "ws://127.0.0.1:7790"), "the role is signed");
        assert_eq!(j.message(&signing(), &[0; 3]), Ok(Step::Wait), "a short message is waited past");
        let joined = json!({"type": "joined", "role": "host"});
        assert_eq!(j.message(&signing(), &control(&joined)).unwrap(), Step::Joined(joined));
        assert!(j.message(&signing(), &challenge()).is_err(), "a second challenge after joining is not answered");
    }

    /// R-RELAY-1: `wss://Relay.krowk.com:443` signs as `wss://relay.krowk.com`,
    /// and a viewer's join says so.
    #[test]
    fn r_relay_1_a_wss_dial_signs_its_canonical_origin() {
        let (_, v) = join_sent("wss://Relay.krowk.com:443", RELAY_ROLE_VIEWER, json!({"stream": "cd".repeat(16), "afterSeq": 7}));
        assert_eq!(v["role"], "viewer");
        assert_eq!(v["afterSeq"], 7);
        assert!(verifies(&v, RELAY_ROLE_VIEWER, "wss://relay.krowk.com"));
        assert!(Joining::new("relay.krowk.com", SESSION, "production", DeviceId([1; 16]), RELAY_ROLE_VIEWER, json!({})).is_err_and(|e| e == "relay.krowk.com has no origin a relay signs"));
    }

    /// R-RELAY-1: the ticket rides the header, the env the query, and
    /// production none.
    #[test]
    fn r_relay_1_dial_adds_the_env_but_never_the_ticket() {
        assert_eq!(dial("ws://127.0.0.1:7790/", SESSION, "development", "t0"), Dial { url: format!("ws://127.0.0.1:7790/v1/relay/{SESSION}?env=development"), ticket: "t0".into() });
        assert_eq!(dial("wss://relay.krowk.com", SESSION, "production", "t0").url, format!("wss://relay.krowk.com/v1/relay/{SESSION}"));
    }

    /// R-CLIENT-1: what the relay says in place of the challenge or of
    /// `joined` is the error, in the words the harness has always used.
    #[test]
    fn r_client_1_a_refusal_or_a_challenge_without_a_nonce_is_the_error() {
        let new = || Joining::new("ws://127.0.0.1:7790", SESSION, "production", DeviceId([1; 16]), RELAY_ROLE_VIEWER, json!({})).unwrap();
        let error = control(&json!({"type": "error", "code": "unauthorized", "message": "the ticket has expired"}));
        assert_eq!(new().message(&signing(), &control(&json!({"type": "challenge"}))), Err("the relay's challenge has no nonce".into()));
        assert_eq!(new().message(&signing(), &control(&json!({"type": "challenge", "nonce": "09".repeat(31)}))), Err("the relay's challenge has no nonce".into()));
        assert_eq!(new().message(&signing(), &error), Err("the relay refused: unauthorized — the ticket has expired".into()));
        let mut j = new();
        assert!(matches!(j.message(&signing(), &challenge()), Ok(Step::Send(_))));
        assert_eq!(j.message(&signing(), &error), Err("the relay refused: unauthorized — the ticket has expired".into()));
        assert_eq!(refusal(&json!({"type": "nope"})), "the relay refused: ? — ");
        let mut j = new();
        assert!(matches!(j.message(&signing(), &challenge()), Ok(Step::Send(_))));
        assert_eq!(j.message(&signing(), &challenge()), Err("the relay refused: ? — ".into()), "a second challenge is not signed");
        let mut j = new();
        let mut garbled = control(&json!({}));
        garbled.truncate(HEADER);
        garbled.extend_from_slice(b"{not json");
        assert!(j.message(&signing(), &garbled).is_err_and(|e| e.starts_with("key must be a string")), "serde's words, as before");
        assert_eq!(j.message(&signing(), &challenge()), Err("the join is over".into()), "an error ends the join");
    }

    /// R-CLIENT-1: only kind-4 messages are the relay's; a stream batch
    /// (kind 1) or a short message is waited past.
    #[test]
    fn r_client_1_read_control_ignores_any_other_kind() {
        let mut batch = control(&json!({"type": "challenge", "nonce": e2e::hex(&NONCE)}));
        batch[1] = crate::protocol::frame::KIND_BATCH;
        assert_eq!(read_control(&batch), None);
        assert_eq!(read_control(&challenge()[..HEADER - 1]), None);
        let mut j = Joining::new("ws://127.0.0.1:7790", SESSION, "production", DeviceId([1; 16]), RELAY_ROLE_VIEWER, json!({})).unwrap();
        assert_eq!(j.message(&signing(), &batch), Ok(Step::Wait));
        assert_eq!(j.message(&signing(), &challenge()[..HEADER - 1]), Ok(Step::Wait));
        assert!(matches!(j.message(&signing(), &challenge()), Ok(Step::Send(_))));
    }
}
