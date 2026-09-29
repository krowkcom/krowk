//! Sync: a session running on one device, watched and steered from another
//! (Harness P1 ticket 19; engineering/harness.md → Sync). The host holds
//! the session's lease, streams its frames sealed over the relay and writes
//! its log as sealed chunks (`host`); another device lists synced sessions
//! from their sealed indexes, attaches from the latest checkpoint and the
//! tail, and goes live (`viewer`). What the two say to each other rides
//! `krowk_client::relay_link`'s chains, so the relay and the registry only
//! ever hold ciphertext (R-E2E-1).

pub mod host;
pub mod store;
pub mod viewer;

use futures_util::{SinkExt, StreamExt};
use krowk_client::e2e::{self, DeviceId, SigningKey};
use krowk_client::protocol::frame::{KIND_RELAY, HEADER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How often a link says `ping`, and how long without a word from the
/// relay it counts as gone (relay.md → Heartbeats).
pub const PING: Duration = Duration::from_secs(10);
pub const DEAD: Duration = Duration::from_secs(30);

/// One display frame: a viewer hands on at most one batch of updates per
/// frame (R-LAG-7), and the host seals at most one batch per frame (R-LAG-3).
pub const FRAME: Duration = Duration::from_millis(16);

/// What a device joins a channel as.
pub struct Join<'a> {
    pub relay: &'a str,
    pub session: &'a str,
    pub env: &'a str,
    pub ticket: &'a str,
    pub device: DeviceId,
    pub signing: &'a SigningKey,
    pub role: u8,
    /// The rest of the join: a host's `fence` and `stream`, a viewer's
    /// `stream` and `afterSeq`.
    pub extra: Value,
}

/// Dials the relay with the ticket in `X-Krowk-Ticket`, answers its
/// challenge with a signature over the origin dialed, and joins. Answers
/// the link and `joined`, or the relay's refusal.
pub async fn join(j: Join<'_>) -> Result<(Ws, Value), String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let base = j.relay.trim_end_matches('/');
    let query = if j.env == "production" { String::new() } else { format!("?env={}", j.env) };
    let mut req = format!("{base}/v1/relay/{}{query}", j.session).into_client_request().map_err(|e| format!("{base} is not a relay URL: {e}"))?;
    req.headers_mut().insert("x-krowk-ticket", j.ticket.parse().map_err(|_| "the relay ticket is not a header value")?);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(req)).await.map_err(|_| "the relay did not answer in 10 seconds".to_string())?.map_err(|e| format!("the relay could not be reached: {e}"))?;
    let challenge = next_control(&mut ws).await?;
    if challenge["type"] != "challenge" {
        return Err(refusal(&challenge));
    }
    let nonce: [u8; 32] = challenge["nonce"].as_str().and_then(e2e::unhex).and_then(|v| v.try_into().ok()).ok_or("the relay's challenge has no nonce")?;
    let origin = e2e::canonical_origin(base).ok_or_else(|| format!("{base} has no origin a relay signs"))?;
    let session = crate::daemon::ws::uuid(j.session);
    let sig = j.signing.sign_relay_join(j.role, &session, &nonce, &j.device, &origin).map_err(|e| e.to_string())?;
    let mut msg = json!({"type": "join", "role": if j.role == e2e::RELAY_ROLE_HOST { "host" } else { "viewer" }, "device": j.device.to_string(), "signature": e2e::hex(&sig), "env": j.env});
    if let Value::Object(extra) = j.extra {
        for (k, v) in extra {
            msg[k] = v;
        }
    }
    ws.send(Message::Binary(crate::relay::control(&msg).into())).await.map_err(|e| e.to_string())?;
    let joined = next_control(&mut ws).await?;
    if joined["type"] != "joined" {
        return Err(refusal(&joined));
    }
    Ok((ws, joined))
}

fn refusal(v: &Value) -> String {
    format!("the relay refused: {} — {}", v["code"].as_str().unwrap_or("?"), v["message"].as_str().unwrap_or(""))
}

async fn next_control(ws: &mut Ws) -> Result<Value, String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Err(_) => return Err("the relay said nothing for 10 seconds".into()),
            Ok(None) | Ok(Some(Err(_))) => return Err("the relay closed the connection".into()),
            Ok(Some(Ok(Message::Binary(b)))) if b.len() >= HEADER && b[1] == KIND_RELAY => return serde_json::from_slice(&b[HEADER..]).map_err(|e| e.to_string()),
            Ok(Some(Ok(Message::Close(_)))) => return Err("the relay closed the connection".into()),
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// One message off a link.
pub enum In {
    Control(Value),
    Envelope(Vec<u8>),
    /// A heartbeat's answer, or anything else that says the link lives.
    Alive,
    Closed,
}

pub async fn recv(ws: &mut Ws) -> In {
    match ws.next().await {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => In::Closed,
        Some(Ok(Message::Binary(b))) if b.len() >= HEADER && b[1] == KIND_RELAY => serde_json::from_slice(&b[HEADER..]).map_or(In::Alive, In::Control),
        Some(Ok(Message::Binary(b))) if b.len() >= HEADER => In::Envelope(b.to_vec()),
        Some(Ok(_)) => In::Alive,
    }
}

pub async fn send(ws: &mut Ws, bytes: Vec<u8>) -> bool {
    ws.send(Message::Binary(bytes.into())).await.is_ok()
}

pub async fn ping(ws: &mut Ws) -> bool {
    ws.send(Message::Text("ping".into())).await.is_ok()
}

/// A viewer's command to the host, sealed in its frames: `id` is the
/// viewer's own, which the host's `ack` names back (R-LAG-8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub command: crate::protocol::Command,
}

/// What the host routes back to one viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Answer {
    /// The command was taken (`error` none) or refused, by the viewer's id.
    Ack { id: String, error: Option<String> },
}

/// The body of the host's welcome to one viewer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    /// The log's head as the host sealed it: what the viewer's reading of
    /// the chunks must reach (the prefix check).
    pub head: Option<store::Head>,
    /// Approvals waiting for an answer, sent again to each viewer that
    /// arrives (R-PERM-2).
    pub approvals: Vec<crate::protocol::ApprovalRequest>,
}

/// A batch's body: the frames the host sent in one display frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Batch {
    pub lines: Vec<crate::protocol::StreamLine>,
    /// The log's head once the chunks written so far are in.
    pub head: Option<store::Head>,
}
