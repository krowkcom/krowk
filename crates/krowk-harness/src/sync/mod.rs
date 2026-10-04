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
    // `wss://`, the hosted relay, is dialed through the harness's own rustls
    // configuration; `ws://` (a local relay, a direct path) needs none.
    let tls = if req.uri().scheme_str() == Some("wss") {
        let config = tokio::task::spawn_blocking(crate::http::tls_config).await.map_err(|e| e.to_string())?.map_err(|e| format!("no TLS to reach the relay with: {e}"))?;
        Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config)))
    } else {
        None
    };
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async_tls_with_config(req, None, false, tls)).await.map_err(|_| "the relay did not answer in 10 seconds".to_string())?.map_err(|e| format!("the relay could not be reached: {e}"))?;
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
/// viewer's own, random per viewer run, which the host's `ack` names back
/// (R-LAG-8) and dedups by, so a command sent again after a reconnect —
/// its ack lost — runs once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub command: crate::protocol::Command,
}

/// What a viewer's frame holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ViewerFrame {
    Command(Remote),
    /// The logged events after `after` (the last this viewer holds; none
    /// for all the host has), answered in this viewer's routed chain: what
    /// a viewer asks after every welcome and on any gap in the stream, so a
    /// batch the relay dropped, a resync or a new stream loses nothing.
    #[serde(rename_all = "camelCase")]
    CatchUp { after: Option<String> },
}

/// What the host routes back to one viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Answer {
    /// The command was taken (`error` none) or refused, by the viewer's id.
    Ack { id: String, error: Option<String> },
    /// Logged events a viewer asked for, in order; `more` when another
    /// page follows.
    CatchUp { events: Vec<crate::protocol::LogEvent>, more: bool },
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
        let mut run = tokio::spawn(host::run(o, std::sync::Arc::new(daemon), stop_rx, cp_rx));
        // A bridge that ends by itself — it could not follow the session,
        // or lost its lease — ends the command then, with why, rather than
        // leaving it silent until someone interrupts it.
        let ended = tokio::select! {
            r = &mut run => r,
            _ = host::interrupted() => {
                let _ = stop.send(true);
                run.await
            }
        };
        ended.map_err(|e| ("sync_failed".to_string(), e.to_string()))?.map_err(|e| ("sync_failed".to_string(), e))
    })
}

/// What a line typed into `krowk sync attach` asks of the session: a
/// prompt, unless it is one of the slash commands `krowk help sync attach`
/// lists. Any other line starting with `/` is a prompt as typed, so a skill
/// (`/review x`) or a path still reaches the model; `//text` sends `/text`
/// for a prompt that would otherwise read as a command.
pub fn typed(session: &str, line: String) -> Result<crate::protocol::Command, String> {
    use crate::protocol::{ApprovalDecision, Command};
    let session_id = session.to_string();
    let prompt = |text: String| Ok(Command::Prompt { session_id: Some(session.to_string()), text, images: Vec::new(), model: None, permission_mode: Default::default(), toolset: None, effort: None, budget: None });
    let trimmed = line.trim();
    if let Some(literal) = trimmed.strip_prefix("//") {
        return prompt(format!("/{literal}"));
    }
    let Some(rest) = trimmed.strip_prefix('/') else { return prompt(line) };
    let (word, arg) = rest.split_once(char::is_whitespace).map(|(w, a)| (w, a.trim())).unwrap_or((rest, ""));
    let decision = match word {
        "approve" => ApprovalDecision::Allow,
        "allow-session" => ApprovalDecision::AllowSession,
        "deny" => ApprovalDecision::Deny,
        "interrupt" if arg.is_empty() => return Ok(Command::Interrupt { session_id }),
        "steer" if !arg.is_empty() => return Ok(Command::Steer { session_id, text: arg.to_string(), images: Vec::new() }),
        "interrupt" | "steer" => return Err(format!("`/{word}` is `/interrupt` alone, or `/steer TEXT`")),
        _ => return prompt(line),
    };
    if arg.is_empty() || arg.contains(char::is_whitespace) {
        return Err(format!("`/{word}` takes the requestId an approval.requested line names"));
    }
    Ok(Command::Approve { session_id, request_id: arg.to_string(), decision })
}

/// `krowk sync attach`: follows a synced session, writing each update as
/// one JSON line to `out`, and sends each line of stdin as a prompt or
/// the command it names (`typed`). Stdin's end ends it, once every command
/// sent has been answered: what a script piped in is taken before it goes,
/// and nothing waits on a person who is no longer typing.
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
        let mut owed = Owed::default();
        loop {
            tokio::select! {
                // The session first: stdin closed at once (`</dev/null`)
                // still prints what was read on attaching before it ends.
                biased;
                u = v.updates.recv() => {
                    let Some(batch) = u else { return Ok(()) };
                    for u in batch {
                        owed.saw(&u);
                        let line = jsonl(u)?;
                        if !line.is_empty() {
                            let _ = writeln!(out, "{line}");
                        }
                    }
                    let _ = out.flush();
                }
                l = lines.recv(), if !owed.eof => match l {
                    Some(text) if !text.trim().is_empty() => match typed(&session, text) {
                        Ok(c) => {
                            owed.handed += 1;
                            let _ = v.commands.send(c);
                        }
                        Err(e) => eprintln!("krowk: {e}"),
                    },
                    Some(_) => {}
                    None => owed.eof = true,
                },
            }
            if owed.done() {
                return Ok(());
            }
        }
    })
}

/// The commands `krowk sync attach` sent and has no answer to yet, and
/// whether stdin has ended: once it has and none is owed, it is done.
#[derive(Default)]
struct Owed {
    eof: bool,
    /// Handed to the viewer, not yet shown as sent.
    handed: usize,
    /// Sent (or queued) and not yet acknowledged, by id.
    unacked: std::collections::HashSet<String>,
}

impl Owed {
    fn saw(&mut self, u: &viewer::Update) {
        match u {
            viewer::Update::Sent { id, .. } => {
                self.handed = self.handed.saturating_sub(1);
                self.unacked.insert(id.clone());
            }
            viewer::Update::Acked { id, .. } => {
                self.unacked.remove(id);
            }
            _ => {}
        }
    }

    fn done(&self) -> bool {
        self.eof && self.handed == 0 && self.unacked.is_empty()
    }
}

/// One update as `krowk sync attach` prints it: a stream line as the
/// protocol has it, the session's events one a line, or a `sync.*` line of
/// the viewer's own. A note goes to stderr and prints nothing.
fn jsonl(u: viewer::Update) -> Result<String, String> {
    Ok(match u {
        viewer::Update::Line(l) => {
            // Said once per request, beside the line a script reads: what
            // answers it, typed here.
            if let crate::protocol::StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)) = &l {
                eprintln!("krowk: {} wants approval — type `/approve {id}`, `/allow-session {id}` or `/deny {id}`", r.tool, id = r.request_id);
            }
            serde_json::to_string(&l).unwrap_or_default()
        }
        viewer::Update::Attached { events, .. } | viewer::Update::CaughtUp(events) => events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"),
        viewer::Update::Host(h) => json!({"type": "sync.host", "present": h}).to_string(),
        viewer::Update::Sent { id, queued } => json!({"type": "sync.sent", "id": id, "queued": queued}).to_string(),
        viewer::Update::Acked { id, error } => json!({"type": "sync.acked", "id": id, "error": error}).to_string(),
        viewer::Update::Path { path, via } => json!({"type": "sync.path", "path": path, "via": via}).to_string(),
        viewer::Update::Gap => json!({"type": "sync.gap"}).to_string(),
        viewer::Update::Note(n) => {
            eprintln!("krowk: {n}");
            String::new()
        }
        viewer::Update::Failed(e) => return Err(e),
    })
}

#[cfg(test)]
mod tests {
    use super::typed;
    use crate::protocol::{ApprovalDecision, Command};

    fn prompt_text(c: Command) -> String {
        match c {
            Command::Prompt { text, session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("s"));
                text
            }
            other => panic!("not a prompt: {other:?}"),
        }
    }

    /// R-RELAY-1: the hosted relay is reachable only at `wss://`, so a
    /// `wss://` relay URL is dialed over TLS — the listener's first bytes are
    /// a TLS ClientHello (record type 0x16), not the build refusing the URL
    /// before it connects, which left v0.12.0-rc3's host and viewers off the
    /// production relay without a word.
    #[test]
    fn r_relay_1_a_wss_relay_is_dialed_over_tls() {
        use tokio::io::AsyncReadExt;
        let rt = super::runtime().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay = format!("wss://{}", listener.local_addr().unwrap());
            let first = tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut b = [0u8; 3];
                s.read_exact(&mut b).await.unwrap();
                b
            });
            let signing = krowk_client::e2e::SigningKey::from_secret(&[7; 32]).unwrap();
            let j = super::Join { relay: &relay, session: "01a0f731-f1c0-7000-92d7-64b247e51781", env: "production", ticket: "00", device: krowk_client::e2e::DeviceId([1; 16]), signing: &signing, role: krowk_client::e2e::RELAY_ROLE_VIEWER, extra: serde_json::json!({}) };
            let (joined, b) = tokio::join!(super::join(j), tokio::time::timeout(std::time::Duration::from_secs(10), first));
            let b = b.expect("the wss dial reached the listener").unwrap();
            assert_eq!(b[..2], [0x16, 0x03], "a TLS handshake record, got {b:?}");
            let e = joined.map(|_| ()).expect_err("no relay answered");
            assert!(!e.contains("TLS support not compiled in"), "{e}");
        });
    }

    /// The five commands, each to its `Command`; anything else starting
    /// with `/` is a prompt as typed, and `//` escapes a command's name.
    #[test]
    fn typed_lines_are_the_five_commands_or_else_prompts() {
        let t = |l: &str| typed("s", l.to_string());
        for (line, want) in [("/approve r1", ApprovalDecision::Allow), ("/allow-session r1", ApprovalDecision::AllowSession), ("/deny  r1 ", ApprovalDecision::Deny)] {
            let Ok(Command::Approve { session_id, request_id, decision }) = t(line) else { panic!("{line}") };
            assert_eq!((session_id.as_str(), request_id.as_str(), decision), ("s", "r1", want), "{line}");
        }
        assert!(matches!(t("/interrupt"), Ok(Command::Interrupt { session_id }) if session_id == "s"));
        assert!(matches!(t("/steer go on"), Ok(Command::Steer { text, .. }) if text == "go on"));
        for bad in ["/approve", "/deny a b", "/interrupt now", "/steer"] {
            assert!(t(bad).is_err(), "{bad}");
        }
        assert_eq!(prompt_text(t("say hi").unwrap()), "say hi");
        assert_eq!(prompt_text(t("/review x").unwrap()), "/review x");
        assert_eq!(prompt_text(t("/usr/bin is where").unwrap()), "/usr/bin is where");
        assert_eq!(prompt_text(t("//approve r1").unwrap()), "/approve r1");
        assert_eq!(prompt_text(t("//etc").unwrap()), "/etc");
    }
}
