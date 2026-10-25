//! Sync: a session running on one device, watched and steered from another
//! (Harness P1 ticket 19; engineering/harness.md → Sync). The host holds
//! the session's lease, streams its frames sealed over the relay and writes
//! its log as sealed chunks (`host`); another device lists synced sessions
//! from their sealed indexes, attaches from the latest checkpoint and the
//! tail, and goes live (`viewer`). What the two say to each other rides
//! `krowk_client::relay_link`'s chains, so the relay and the registry only
//! ever hold ciphertext (R-E2E-1).

pub mod announce;
pub mod auto;
pub mod commands;
pub mod direct;
pub mod handoff;
pub mod host;
pub mod hosts;
pub mod mdns;
pub mod store;
pub mod tailscale;
pub mod viewer;

use futures_util::{SinkExt, StreamExt};
use krowk_client::e2e::{DeviceId, SigningKey};
use krowk_client::protocol::frame::{KIND_RELAY, HEADER};
use krowk_client::sync::join as handshake;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The heartbeat (`PING`, `DEAD`) and the display frame (`FRAME`), shared
/// by every client (R-CLIENT-1): `krowk_client::sync` declares them.
pub use krowk_client::sync::{DEAD, FRAME, PING};

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
///
/// The handshake itself — the URL, the challenge, the signature, the join
/// and its answer — is `krowk_client::sync::join`'s, which every client
/// links (R-CLIENT-1); this is its IO: the socket, TLS and the waits.
pub async fn join(j: Join<'_>) -> Result<(Ws, Value), String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let dial = handshake::dial(j.relay, j.session, j.env, j.ticket);
    let mut req = dial.url.into_client_request().map_err(|e| format!("{} is not a relay URL: {e}", j.relay.trim_end_matches('/')))?;
    req.headers_mut().insert("x-krowk-ticket", dial.ticket.parse().map_err(|_| "the relay ticket is not a header value")?);
    let mut joining = handshake::Joining::new(j.relay, j.session, j.env, j.device, j.role, j.extra)?;
    // `wss://`, the hosted relay, is dialed through the harness's own rustls
    // configuration; `ws://` (a local relay, a direct path) needs none.
    let tls = if req.uri().scheme_str() == Some("wss") {
        let config = tokio::task::spawn_blocking(crate::http::tls_config).await.map_err(|e| e.to_string())?.map_err(|e| format!("no TLS to reach the relay with: {e}"))?;
        Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config)))
    } else {
        None
    };
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async_tls_with_config(req, None, false, tls)).await.map_err(|_| "the relay did not answer in 10 seconds".to_string())?.map_err(|e| format!("the relay could not be reached: {e}"))?;
    loop {
        match joining.message(j.signing, &next_binary(&mut ws).await?)? {
            handshake::Step::Wait => {}
            handshake::Step::Send(b) => ws.send(Message::Binary(b.into())).await.map_err(|e| e.to_string())?,
            handshake::Step::Joined(joined) => return Ok((ws, joined)),
        }
    }
}

/// The next binary message, each waited on for 10 seconds.
async fn next_binary(ws: &mut Ws) -> Result<Vec<u8>, String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Err(_) => return Err("the relay said nothing for 10 seconds".into()),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return Err("the relay closed the connection".into()),
            Ok(Some(Ok(Message::Binary(b)))) => return Ok(b.to_vec()),
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
        Some(Ok(Message::Binary(b))) if b.len() >= HEADER && b[1] == KIND_RELAY => handshake::read_control(&b).map_or(In::Alive, In::Control),
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

/// The wire shapes a viewer and its host send each other, shared by every
/// client (R-CLIENT-1): `krowk_client::sync` declares them.
pub use krowk_client::sync::{Answer, Batch, Handed, Remote, ViewerFrame, Welcome};

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("the async runtime could not start: {e}"))
}

/// `krowk sync host`: reaches this machine's daemon (starting it when none
/// answers) as a client that answers approvals, and runs the bridge until
/// Ctrl-C or SIGTERM. Errors are `(code, message)`.
pub fn run_host(o: host::Options, env: &dyn Fn(&str) -> String, cwd: &std::path::Path, version: &str, spawn: &crate::daemon::Spawn<'_>) -> Result<(), (String, String)> {
    let rt = runtime().map_err(|e| ("runtime_unavailable".to_string(), e))?;
    let ended = rt.block_on(async move {
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
        let code = |e: &str| if e.starts_with(host::LIST_MOVED) { "device_list_moved" } else { "sync_failed" }.to_string();
        match ended.map_err(|e| ("sync_failed".to_string(), e.to_string()))? {
            // Handed over: done here, as asked.
            Err(e) if e.starts_with(host::HANDED_OVER) => {
                eprintln!("krowk: {e}");
                Ok(())
            }
            r => r.map_err(|e| (code(&e), e)),
        }
    });
    // The lease is given back by now; a notification still on its way
    // (`host::Hosting::notify`, fire and forget) never holds up the exit.
    rt.shutdown_background();
    ended
}

/// `krowk sync take`'s ask: attached to the session as a viewer, its host
/// asked to hand it to this device, and its answer — what to take the
/// session up with, or why it stays. A host that does not answer in `wait`
/// is a refusal too.
pub fn ask_handoff(o: viewer::Options, wait: Duration) -> Result<Handed, String> {
    runtime()?.block_on(async move {
        let mut v = viewer::attach(o).await?;
        let _ = v.handoff.send(());
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let batch = tokio::time::timeout_at(deadline, v.updates.recv()).await.map_err(|_| format!("the host did not answer in {} seconds; the session stays where it was", wait.as_secs()))?.ok_or("the session's link closed before the host answered")?;
            for u in batch {
                match u {
                    viewer::Update::Handoff(r) => return r,
                    viewer::Update::Failed(e) => return Err(e),
                    _ => {}
                }
            }
        }
    })
}

/// What a line typed into `krowk sync attach` asks of the session.
#[derive(Debug)]
pub enum Typed {
    /// Sent as it is.
    Command(crate::protocol::Command),
    /// `/switch`: the rate-limit offer the attach loop holds, taken
    /// (`Offered::switch`).
    Switch,
}

/// What a line typed into `krowk sync attach` asks of the session: a
/// prompt, unless it is one of the slash commands `krowk help sync attach`
/// lists. Any other line starting with `/` is a prompt as typed, so a skill
/// (`/review x`) or a path still reaches the model; `//text` sends `/text`
/// for a prompt that would otherwise read as a command.
pub fn typed(session: &str, line: String) -> Result<Typed, String> {
    use crate::protocol::{ApprovalDecision, Command};
    let session_id = session.to_string();
    let prompt = |text: String| Ok(Typed::Command(prompt(session, text)));
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
        "interrupt" if arg.is_empty() => return Ok(Typed::Command(Command::Interrupt { session_id })),
        // One of the session's subagents, by the id its `subagent.started`
        // line names (R-SUB-12): the host refuses any other.
        "interrupt" if crate::log::valid_id(arg) => return Ok(Typed::Command(Command::Interrupt { session_id: arg.to_string() })),
        "steer" if !arg.is_empty() => return Ok(Typed::Command(Command::Steer { session_id, text: arg.to_string(), images: Vec::new() })),
        "interrupt" | "steer" => return Err(format!("`/{word}` is `/interrupt` alone or with a subagent's session id, or `/steer TEXT`")),
        // The `<instance>/<id>` form `--model` takes: the instances are the
        // host's, so the host checks the model against its own (R-CRED-1).
        "model" => {
            return match arg.split_once('/') {
                Some((instance, model)) if !instance.is_empty() && !model.is_empty() && !arg.contains(char::is_whitespace) => Ok(Typed::Command(Command::SwitchModel { session_id: Some(session_id), model: crate::protocol::ModelRef { instance: instance.into(), model: model.into() } })),
                _ => Err("`/model` takes the model as `<instance>/<id>`, e.g. `/model anthropic/claude-opus-5-5`".into()),
            };
        }
        "switch" if arg.is_empty() => return Ok(Typed::Switch),
        "switch" => return Err("`/switch` takes nothing: it takes the switch a rate limit offered".into()),
        _ => return prompt(line),
    };
    if arg.is_empty() || arg.contains(char::is_whitespace) {
        return Err(format!("`/{word}` takes the requestId an approval.requested line names"));
    }
    Ok(Typed::Command(Command::Approve { session_id, request_id: arg.to_string(), decision, answers: Vec::new() }))
}

/// A prompt from `krowk sync attach`: the session's model, settings and
/// mode, the host's to choose.
fn prompt(session: &str, text: String) -> crate::protocol::Command {
    crate::protocol::Command::Prompt { session_id: Some(session.to_string()), text, images: Vec::new(), model: None, permission_mode: Default::default(), toolset: None, effort: None, budget: None }
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
        let mut offered = Offered::default();
        loop {
            tokio::select! {
                // The session first: stdin closed at once (`</dev/null`)
                // still prints what was read on attaching before it ends.
                biased;
                u = v.updates.recv() => {
                    let Some(batch) = u else { return Ok(()) };
                    for u in batch {
                        if let Some(next) = owed.saw(&u) {
                            owed.hand(&v.commands, next, None);
                        }
                        offered.saw(&session, &u);
                        let line = jsonl(u)?;
                        if !line.is_empty() {
                            let _ = writeln!(out, "{line}");
                        }
                    }
                    let _ = out.flush();
                }
                l = lines.recv(), if !owed.eof => match l {
                    Some(text) if !text.trim().is_empty() => match typed(&session, text) {
                        Ok(Typed::Command(c)) => {
                            offered.sending(&c);
                            owed.hand(&v.commands, c, None);
                        }
                        Ok(Typed::Switch) => match offered.switch(&session) {
                            Ok((switch, again)) => owed.hand(&v.commands, switch, again),
                            Err(e) => eprintln!("krowk: {e}"),
                        },
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
    /// Handed to the viewer, not yet shown as sent, in the order handed,
    /// each with the command to send once the host has taken it.
    handed: std::collections::VecDeque<Option<crate::protocol::Command>>,
    /// Sent (or queued) and not yet acknowledged, by id.
    unacked: std::collections::HashSet<String>,
    /// What follows a sent command once the host takes it, by its id.
    then: std::collections::HashMap<String, crate::protocol::Command>,
}

impl Owed {
    /// `c` to the viewer, and `then` once the host has taken it: the host
    /// runs a viewer's commands side by side, so a prompt sent with the
    /// switch before it could start on the model it switches from.
    fn hand(&mut self, commands: &tokio::sync::mpsc::UnboundedSender<crate::protocol::Command>, c: crate::protocol::Command, then: Option<crate::protocol::Command>) {
        if commands.send(c).is_ok() {
            self.handed.push_back(then);
        }
    }

    /// Notes `u`, and answers the command to send now: one that waited on
    /// the command just taken. A refused one's never goes.
    fn saw(&mut self, u: &viewer::Update) -> Option<crate::protocol::Command> {
        match u {
            viewer::Update::Sent { id, .. } => {
                if let Some(then) = self.handed.pop_front().flatten() {
                    self.then.insert(id.clone(), then);
                }
                self.unacked.insert(id.clone());
                None
            }
            viewer::Update::Acked { id, error } => {
                self.unacked.remove(id);
                self.then.remove(id).filter(|_| error.is_none())
            }
            _ => None,
        }
    }

    fn done(&self) -> bool {
        self.eof && self.handed.is_empty() && self.unacked.is_empty()
    }
}

/// The rate-limit offer `krowk sync attach` last saw for its session, and
/// the last prompt it sent, which `/switch` sends again on the instance
/// offered — the terminal's yes (R-INST-7, R-SWITCH-4).
#[derive(Default)]
struct Offered {
    /// The offer, and the turn that hit the limit.
    offer: Option<(String, crate::protocol::SwitchOffer)>,
    last_prompt: Option<String>,
    /// The session's latest turns' prompts, each its first text from the
    /// person as the log has it (a skill's instructions, a hook's context,
    /// steering are logged after it): the limited turn is this viewer's
    /// when its prompt is `last_prompt`.
    asked: std::collections::VecDeque<(String, String)>,
}

/// Turns `Offered` keeps the prompt of: enough for a catch-up that
/// arrives after the result of the turn it brings the start of.
const ASKED: usize = 8;

impl Offered {
    /// A result of the session's own turn replaces the offer, said on
    /// stderr when there is one; another turn of the session starting, as
    /// it happens, withdraws it.
    fn saw(&mut self, session: &str, u: &viewer::Update) {
        use crate::protocol::{LiveEvent, StreamLine};
        match u {
            viewer::Update::Line(StreamLine::Live(LiveEvent::Result(r))) if r.session_id == session => {
                self.offer = r.switch_offer.clone().map(|o| (r.turn_id.clone(), o));
                if let Some((_, o)) = &self.offer {
                    eprintln!("krowk: {} is at its limit — type /switch to continue on {}", o.from, o.to);
                }
            }
            viewer::Update::Line(StreamLine::Log(e)) => self.logged(session, e, true),
            // A rejoin's catch-up brings what the stream missed, which may
            // come after the result it went before.
            viewer::Update::Attached { events, .. } | viewer::Update::CaughtUp(events) => {
                for e in events.iter().filter_map(|e| serde_json::from_value::<crate::protocol::LogEvent>(e.clone()).ok()) {
                    self.logged(session, &e, false);
                }
            }
            _ => {}
        }
    }

    fn logged(&mut self, session: &str, e: &crate::protocol::LogEvent, live: bool) {
        use crate::protocol::{Item, LogBody};
        if e.session_id != session {
            return;
        }
        match &e.body {
            LogBody::TurnStarted { turn_id, .. } if live && self.offer.as_ref().is_some_and(|(t, _)| t != turn_id) => self.offer = None,
            LogBody::ItemCompleted { turn_id, item: Item::UserText { text, .. }, .. } if !self.asked.iter().any(|(t, _)| t == turn_id) => {
                if self.asked.len() == ASKED {
                    self.asked.pop_front();
                }
                self.asked.push_back((turn_id.clone(), text.clone()));
            }
            _ => {}
        }
    }

    /// A prompt sent from here is the one `/switch` sends again, and
    /// answers the offer with a no.
    fn sending(&mut self, c: &crate::protocol::Command) {
        if let crate::protocol::Command::Prompt { text, .. } = c {
            self.last_prompt = Some(text.clone());
            self.offer = None;
        }
    }

    /// `/switch`: the switch to the offer's instance, and the prompt to
    /// send again once the host has taken it — none when the limited turn
    /// was not prompted from here, another device's.
    fn switch(&mut self, session: &str) -> Result<(crate::protocol::Command, Option<crate::protocol::Command>), String> {
        let (turn, offer) = self.offer.take().ok_or("no switch offer is waiting")?;
        let switch = crate::protocol::Command::SwitchModel { session_id: Some(session.to_string()), model: offer.to.clone() };
        let asked = self.asked.iter().find(|(t, _)| *t == turn).map(|(_, text)| text);
        let again = self.last_prompt.clone().filter(|p| asked == Some(p));
        if again.is_none() {
            eprintln!("krowk: switching to {} — the limited turn was not prompted from here, so type the prompt to run there", offer.to);
        }
        Ok((switch, again.map(|text| prompt(session, text))))
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
            match &l {
                // Questions are answered in the TUI, which can pick and type.
                crate::protocol::StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)) if !r.questions.is_empty() => {
                    eprintln!("krowk: {} asks a question — answer it in krowk's TUI, or type `/deny {id}` to decline", r.tool, id = r.request_id);
                }
                crate::protocol::StreamLine::Live(crate::protocol::LiveEvent::ApprovalRequested(r)) => {
                    eprintln!("krowk: {} wants approval — type `/approve {id}`, `/allow-session {id}` or `/deny {id}`", r.tool, id = r.request_id);
                }
                _ => {}
            }
            serde_json::to_string(&l).unwrap_or_default()
        }
        viewer::Update::Attached { events, .. } | viewer::Update::CaughtUp(events) => events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"),
        viewer::Update::Host(h) => json!({"type": "sync.host", "present": h}).to_string(),
        viewer::Update::Sent { id, queued } => json!({"type": "sync.sent", "id": id, "queued": queued}).to_string(),
        viewer::Update::Acked { id, error } => json!({"type": "sync.acked", "id": id, "error": error}).to_string(),
        viewer::Update::Path { path, via } => json!({"type": "sync.path", "path": path, "via": via}).to_string(),
        viewer::Update::Gap => json!({"type": "sync.gap"}).to_string(),
        // `krowk sync attach` never asks for one; the line is for whoever does.
        viewer::Update::Handoff(r) => json!({"type": "sync.handoff", "error": r.err()}).to_string(),
        viewer::Update::Note(n) => {
            eprintln!("krowk: {n}");
            String::new()
        }
        viewer::Update::Failed(e) => return Err(e),
        // The terminal keeps no place across runs: nothing to print.
        viewer::Update::Saved(_) => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::{typed, Typed};
    use crate::protocol::{ApprovalDecision, Command};

    fn prompt_text(c: Typed) -> String {
        match c {
            Typed::Command(Command::Prompt { text, session_id, .. }) => {
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

    /// R-CLIENT-1, R-RELAY-1: the control message a client sends is the
    /// one the reference relay builds and reads, byte for byte.
    #[test]
    fn r_client_1_the_clients_control_message_is_the_relays() {
        let v = serde_json::json!({"type": "join", "role": "viewer", "afterSeq": 3});
        let b = super::handshake::control(&v);
        assert_eq!(b, crate::relay::control(&v));
        assert_eq!(super::handshake::read_control(&b), Some(v));
    }

    /// The commands, each to its `Command`, and `/switch` to the marker
    /// the attach loop takes the offer on; anything else starting with `/`
    /// is a prompt as typed, and `//` escapes a command's name.
    /// `/interrupt` takes a subagent's session id, and nothing else.
    #[test]
    fn typed_lines_are_the_commands_or_else_prompts() {
        let t = |l: &str| typed("s", l.to_string()).map(|c| match c {
            Typed::Command(c) => c,
            Typed::Switch => panic!("{l} is no /switch"),
        });
        for (line, want) in [("/approve r1", ApprovalDecision::Allow), ("/allow-session r1", ApprovalDecision::AllowSession), ("/deny  r1 ", ApprovalDecision::Deny)] {
            let Ok(Command::Approve { session_id, request_id, decision, .. }) = t(line) else { panic!("{line}") };
            assert_eq!((session_id.as_str(), request_id.as_str(), decision), ("s", "r1", want), "{line}");
        }
        assert!(matches!(t("/interrupt"), Ok(Command::Interrupt { session_id }) if session_id == "s"));
        let child = "01a0ec7b-0000-7000-8000-00000000c41d";
        assert!(matches!(t(&format!("/interrupt {child}")), Ok(Command::Interrupt { session_id }) if session_id == child), "R-SUB-12: a subagent, by its id");
        assert!(matches!(t("/steer go on"), Ok(Command::Steer { text, .. }) if text == "go on"));
        let Ok(Command::SwitchModel { session_id, model }) = t("/model mock/other") else { panic!("/model") };
        assert_eq!((session_id.as_deref(), model.to_string().as_str()), (Some("s"), "mock/other"));
        for bad in ["/approve", "/deny a b", "/interrupt now", "/steer", "/model", "/model opus", "/model /x", "/model a/", "/model a/b c"] {
            assert!(t(bad).is_err(), "{bad}");
        }
        assert!(matches!(typed("s", " /switch ".into()), Ok(Typed::Switch)));
        assert!(typed("s", "/switch now".into()).is_err());
        let p = |l: &str| typed("s", l.to_string()).unwrap();
        assert_eq!(prompt_text(p("say hi")), "say hi");
        assert_eq!(prompt_text(p("/review x")), "/review x");
        assert_eq!(prompt_text(p("/usr/bin is where")), "/usr/bin is where");
        assert_eq!(prompt_text(p("//approve r1")), "/approve r1");
        assert_eq!(prompt_text(p("//switch")), "/switch");
    }

    fn offered(session: &str, turn: &str) -> super::viewer::Update {
        use crate::protocol::{LiveEvent, ModelRef, StreamLine, SwitchOffer};
        let m = |i: &str| ModelRef { instance: i.into(), model: "m".into() };
        let r = crate::protocol::RunResult { session_id: session.into(), turn_id: turn.into(), status: crate::protocol::TurnStatus::Failed, is_error: true, result: String::new(), model: m("a"), usage: Default::default(), cost_usd: None, duration_ms: 0, num_model_calls: 1, error: None, unread_steers: Vec::new(), switch_offer: Some(SwitchOffer { from: m("a"), to: m("b"), resets_at_ms: None }), worktree: None };
        super::viewer::Update::Line(StreamLine::Live(LiveEvent::Result(r)))
    }

    fn event(session: &str, body: crate::protocol::LogBody) -> crate::protocol::LogEvent {
        crate::protocol::LogEvent { id: "e".into(), parent_id: None, session_id: session.into(), time_ms: 0, body }
    }

    fn said(turn: &str, text: &str) -> crate::protocol::LogEvent {
        event("s", crate::protocol::LogBody::ItemCompleted { turn_id: turn.into(), item_id: "i".into(), item: crate::protocol::Item::user(text) })
    }

    fn started(turn: &str) -> crate::protocol::LogEvent {
        use crate::protocol::ModelRef;
        event("s", crate::protocol::LogBody::TurnStarted { turn_id: turn.into(), model: ModelRef { instance: "a".into(), model: "m".into() }, provider: "anthropic".into(), wire_api: crate::protocol::WireApi::AnthropicMessages, permission_mode: Default::default(), effort: None })
    }

    fn live(e: crate::protocol::LogEvent) -> super::viewer::Update {
        super::viewer::Update::Line(crate::protocol::StreamLine::Log(e))
    }

    /// R-INST-7, R-SWITCH-4: `/switch` with no offer waiting is refused;
    /// with one, it is a switch to the offer's instance and the last prompt
    /// sent from here when the limited turn was its, which goes only once
    /// the host has taken the switch, and never when it refused it. A
    /// prompt sent instead answers no; another turn starting withdraws it.
    #[test]
    fn r_switch_4_switch_takes_the_offer_waiting_and_sends_the_prompt_once_the_switch_is_taken() {
        use super::viewer::Update;
        let mut o = super::Offered::default();
        assert_eq!(o.switch("s").unwrap_err(), "no switch offer is waiting");
        o.sending(&super::prompt("s", "do the thing".into()));
        for e in [started("t1"), said("t1", "do the thing"), said("t1", "a hook's context, or steering")] {
            o.saw("s", &live(e));
        }
        o.saw("s", &offered("other", "t1"));
        assert!(o.switch("s").is_err(), "another session's offer is not this one's");
        o.saw("s", &offered("s", "t1"));
        let (switch, again) = o.switch("s").unwrap();
        let Command::SwitchModel { session_id, model } = &switch else { panic!("{switch:?}") };
        assert_eq!((session_id.as_deref(), model.instance.as_str()), (Some("s"), "b"));
        assert_eq!(prompt_text(Typed::Command(again.clone().unwrap())), "do the thing", "the turn's first text, not what followed it");
        assert!(o.switch("s").is_err(), "taken once");
        o.saw("s", &offered("s", "t1"));
        o.sending(&super::prompt("s", "something else".into()));
        assert!(o.switch("s").is_err(), "a prompt instead is a no");

        // Another device's turn: its offer is a switch alone.
        for e in [started("t2"), said("t2", "from the host's terminal")] {
            o.saw("s", &live(e));
        }
        o.saw("s", &offered("s", "t2"));
        assert!(matches!(o.switch("s"), Ok((Command::SwitchModel { .. }, None))), "never this viewer's last prompt again");
        // The next turn starting withdraws it; its own start, caught up
        // after its result, does not.
        o.saw("s", &offered("s", "t2"));
        o.saw("s", &live(started("t3")));
        assert!(o.switch("s").is_err(), "a turn under way withdrew it");
        o.sending(&super::prompt("s", "mine".into()));
        let caught = |es: Vec<crate::protocol::LogEvent>| Update::CaughtUp(es.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect());
        o.saw("s", &offered("s", "t4"));
        o.saw("s", &caught(vec![started("t4"), said("t4", "mine")]));
        assert_eq!(o.switch("s").map(|(_, again)| again.map(|a| prompt_text(Typed::Command(a)))), Ok(Some("mine".into())), "a caught-up start of the limited turn keeps its offer");
        o.saw("s", &offered("s", "t5"));
        o.saw("s", &caught(vec![started("t5"), said("t5", "theirs")]));
        assert!(matches!(o.switch("s"), Ok((_, None))), "a caught-up turn is read too");

        // The prompt waits on the switch's ack, and goes only if taken.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut owed = super::Owed { eof: true, ..Default::default() };
        owed.hand(&tx, switch.clone(), again.clone());
        assert!(rx.try_recv().is_ok() && !owed.done());
        assert!(owed.saw(&Update::Sent { id: "1".into(), queued: false }).is_none());
        let next = owed.saw(&Update::Acked { id: "1".into(), error: None }).expect("the prompt, once the switch is taken");
        owed.hand(&tx, next, None);
        assert!(!owed.done(), "the prompt is owed too");
        owed.saw(&Update::Sent { id: "2".into(), queued: false });
        assert!(owed.saw(&Update::Acked { id: "2".into(), error: None }).is_none());
        assert!(owed.done());
        owed.hand(&tx, switch, again);
        owed.saw(&Update::Sent { id: "3".into(), queued: false });
        assert!(owed.saw(&Update::Acked { id: "3".into(), error: Some("refused".into()) }).is_none(), "a refused switch sends no prompt");
        assert!(owed.done());
    }
}
