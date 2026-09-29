//! Sync: a session running on one device, watched and steered from another
//! (Harness P1 ticket 19; engineering/harness.md → Sync). The host holds
//! the session's lease, streams its frames sealed over the relay and writes
//! its log as sealed chunks (`host`); another device lists synced sessions
//! from their sealed indexes, attaches from the latest checkpoint and the
//! tail, and goes live (`viewer`). What the two say to each other rides
//! `krowk_client::relay_link`'s chains, so the relay and the registry only
//! ever hold ciphertext (R-E2E-1).

pub mod direct;
pub mod host;
pub mod store;
pub mod tailscale;
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
    /// Where else the viewer may reach the host directly (R-NET-1): sealed
    /// in the welcome under the session key and bound to this connection's
    /// challenge, so only the host could have sent it.
    #[serde(default)]
    pub candidates: Vec<direct::Candidate>,
}

/// A batch's body: the frames the host sent in one display frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Batch {
    pub lines: Vec<crate::protocol::StreamLine>,
    /// The log's head once the chunks written so far are in.
    pub head: Option<store::Head>,
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))
}

/// `krowk sync host`: reaches this machine's daemon (starting it when none
/// answers) as a client that answers approvals, and runs the bridge until
/// Ctrl-C or SIGTERM. Errors are `(code, message)`.
pub fn run_host(o: host::Options, env: &dyn Fn(&str) -> String, cwd: &std::path::Path, version: &str, spawn: &crate::daemon::Spawn<'_>) -> Result<(), (String, String)> {
    let rt = runtime().map_err(|e| ("runtime_unavailable".to_string(), e))?;
    rt.block_on(async move {
        let daemon = crate::daemon::ensure(env, cwd, version, true, spawn).await.map_err(|e| (e.code.clone(), e.message.clone()))?;
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let (_cp, cp_rx) = tokio::sync::mpsc::unbounded_channel();
        let run = tokio::spawn(host::run(o, std::sync::Arc::new(daemon), stop_rx, cp_rx));
        host::interrupted().await;
        let _ = stop.send(true);
        run.await.map_err(|e| ("sync_failed".to_string(), e.to_string()))?.map_err(|e| ("sync_failed".to_string(), e))
    })
}

/// `krowk sync attach`: follows a synced session, writing each update as
/// one JSON line to `out`, and sends each line of stdin as a prompt.
pub fn run_attach(o: viewer::Options, out: &mut dyn std::io::Write) -> Result<(), String> {
    let session = o.session.clone();
    runtime()?.block_on(async move {
        let mut v = viewer::attach(o).await?;
        let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            for l in std::io::stdin().lines().map_while(Result::ok) {
                if lines_tx.send(l).is_err() {
                    return;
                }
            }
        });
        loop {
            tokio::select! {
                l = lines.recv() => if let Some(text) = l.filter(|t| !t.trim().is_empty()) {
                    let _ = v.commands.send(crate::protocol::Command::Prompt { session_id: Some(session.clone()), text, model: None, permission_mode: Default::default(), toolset: None, effort: None, budget: None });
                },
                u = v.updates.recv() => {
                    let Some(batch) = u else { return Ok(()) };
                    for u in batch {
                        let line = match u {
                            viewer::Update::Line(l) => serde_json::to_string(&l).unwrap_or_default(),
                            viewer::Update::Attached { events, .. } | viewer::Update::CaughtUp(events) => events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"),
                            viewer::Update::Host(h) => json!({"type": "sync.host", "present": h}).to_string(),
                            viewer::Update::Sent { id, queued } => json!({"type": "sync.sent", "id": id, "queued": queued}).to_string(),
                            viewer::Update::Acked { id, error } => json!({"type": "sync.acked", "id": id, "error": error}).to_string(),
                            viewer::Update::Path { path, via } => json!({"type": "sync.path", "path": path, "via": via}).to_string(),
                            viewer::Update::Failed(e) => return Err(e),
                        };
                        if !line.is_empty() {
                            let _ = writeln!(out, "{line}");
                        }
                    }
                    let _ = out.flush();
                }
            }
        }
    })
}
