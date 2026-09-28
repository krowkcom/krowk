//! The daemon's WebSocket listener: the protocol's third transport
//! (R-PROTO-1), the one the relay (ticket 17) and a direct connection from
//! another device carry. It is a second listener on the same daemon — the
//! same hubs, the same outboxes, the same `hello`, `execute` and `attach` —
//! only the framing differs.
//!
//! Loopback only, off unless `KROWK_HOST_WS` or `host.websocket` names an
//! address: TCP cannot say whose process connected, so a client proves it
//! is this user with the daemon's token (`host.token`, `0600` beside the
//! socket) in its hello. That stands in until the relay and end-to-end
//! encryption (tickets 15 and 17) replace it.
//!
//! **Frames.** Every WebSocket message is binary: a 28-byte header, then the
//! payload.
//!
//! | offset | size | field | |
//! |---|---|---|---|
//! | 0 | 1 | `v` | 1 |
//! | 1 | 1 | `kind` | 1 batch (daemon → client), 2 frame (client → daemon), 3 ack (client → daemon) |
//! | 2 | 1 | `flags` | bit 0: the payload is zstd-compressed |
//! | 3 | 1 | `enc` | 0 none; 1 is reserved for XChaCha20-Poly1305 (ticket 15), applied after compressing |
//! | 4 | 16 | `session` | the session's UUID, all zero for none |
//! | 20 | 8 | `seq` | big-endian: a batch's last `line.seq`, or an ack's count of batches applied |
//!
//! A batch's payload is the frames the unix socket would send for one
//! session, byte for byte — JSON objects, each with its newline — so the
//! one generated schema describes both. A client frame's payload is one
//! `ClientFrame`.
//!
//! **Flow.** A batch goes out at most every `BATCH_WINDOW` — time, never
//! content (R-LAG-3) — or sooner once `BATCH_BYTES` wait, one message per
//! session with something to send. At most `WINDOW` batches are
//! unacknowledged at once (R-LAG-4); the client acknowledges cumulatively,
//! as often as it likes, and a client that does not is simply sent nothing
//! more until its outbox falls behind and it is caught up from its cursor.
//! Pings go out every `HEARTBEAT` from a task of their own, never queued
//! behind a batch or a window, and a connection nothing is heard on for
//! three of them is closed (R-LAG-9).

use super::outbox::Outbox;
use super::server::{self, Shared};
use crate::protocol::ClientFrame;
use futures_util::{SinkExt, StreamExt};
use std::cell::Cell;
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// The file beside the socket that holds the WebSocket token.
pub const TOKEN: &str = "host.token";

pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// How long a batch collects before it goes: one display frame at 60 Hz.
pub const BATCH_WINDOW: Duration = Duration::from_millis(16);

/// A batch goes at once when this much waits.
pub const BATCH_BYTES: usize = 64 * 1024;

/// Batches in flight before the daemon waits for an ack.
pub const WINDOW: u64 = 16;

/// The largest message a client may send: a prompt with a long paste.
const MAX_IN: usize = 4 << 20;

/// How long a client has to finish the handshake and say hello.
const HELLO_WAIT: Duration = Duration::from_secs(5);

pub const HEADER: usize = 28;
pub const V: u8 = 1;
pub const KIND_BATCH: u8 = 1;
pub const KIND_FRAME: u8 = 2;
pub const KIND_ACK: u8 = 3;
pub const FLAG_ZSTD: u8 = 1;
pub const ENC_NONE: u8 = 0;

/// One WebSocket message of the protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub kind: u8,
    pub flags: u8,
    pub enc: u8,
    pub session: [u8; 16],
    pub seq: u64,
    pub payload: Vec<u8>,
}

impl Envelope {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER + self.payload.len());
        v.extend_from_slice(&[V, self.kind, self.flags, self.enc]);
        v.extend_from_slice(&self.session);
        v.extend_from_slice(&self.seq.to_be_bytes());
        v.extend_from_slice(&self.payload);
        v
    }

    /// Reads one; refused when it is short or of another version.
    pub fn decode(b: &[u8]) -> Result<Envelope, String> {
        if b.len() < HEADER {
            return Err(format!("a frame of {} bytes is shorter than its {HEADER}-byte header", b.len()));
        }
        if b[0] != V {
            return Err(format!("frame version {} is not {V}", b[0]));
        }
        let mut session = [0u8; 16];
        session.copy_from_slice(&b[4..20]);
        let seq = u64::from_be_bytes(b[20..28].try_into().expect("eight bytes"));
        Ok(Envelope { kind: b[1], flags: b[2], enc: b[3], session, seq, payload: b[28..].to_vec() })
    }

    /// The payload, decompressed when it says it is compressed, and never
    /// past `max` bytes however small it came.
    pub fn plain(&self, max: usize) -> Result<Vec<u8>, String> {
        if self.enc != ENC_NONE {
            return Err(format!("encryption {} is not one this daemon speaks", self.enc));
        }
        if self.flags & FLAG_ZSTD == 0 {
            return Ok(self.payload.clone());
        }
        use std::io::Read;
        let d = ruzstd::decoding::StreamingDecoder::new(&self.payload[..]).map_err(|e| format!("the payload is not zstd: {e}"))?;
        let mut out = Vec::new();
        d.take(max as u64 + 1).read_to_end(&mut out).map_err(|e| format!("the payload does not decompress: {e}"))?;
        if out.len() > max {
            return Err(format!("the payload decompresses past {max} bytes"));
        }
        Ok(out)
    }
}

/// A batch of one session's frames: compressed when that makes it smaller
/// (a lone delta does not), then — from ticket 15 — encrypted.
pub fn batch(session: &str, seq: u64, frames: &[u8]) -> Envelope {
    let packed = ruzstd::encoding::compress_to_vec(frames, ruzstd::encoding::CompressionLevel::Fastest);
    let (flags, payload) = if packed.len() < frames.len() { (FLAG_ZSTD, packed) } else { (0, frames.to_vec()) };
    Envelope { kind: KIND_BATCH, flags, enc: ENC_NONE, session: uuid(session), seq, payload }
}

/// A session id's sixteen bytes; zero for none, or one that is no UUID.
pub fn uuid(id: &str) -> [u8; 16] {
    let hex: Vec<u8> = id.bytes().filter(|b| *b != b'-').collect();
    let mut out = [0u8; 16];
    if hex.len() != 32 {
        return out;
    }
    for (i, pair) in hex.chunks(2).enumerate() {
        match std::str::from_utf8(pair).ok().and_then(|p| u8::from_str_radix(p, 16).ok()) {
            Some(b) => out[i] = b,
            None => return [0u8; 16],
        }
    }
    out
}

/// Binds the listener and writes a fresh token beside the socket, `0600`:
/// a daemon's token dies with it.
pub(super) async fn bind(addr: SocketAddr, dir: &Path) -> Result<(TcpListener, String), String> {
    if !addr.ip().is_loopback() {
        return Err(format!("the WebSocket listener binds only to loopback, and {addr} is not"));
    }
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("ws://{addr} cannot be listened on: {e}"))?;
    let token = {
        use ring::rand::SecureRandom;
        let mut raw = [0u8; 32];
        ring::rand::SystemRandom::new().fill(&mut raw).map_err(|_| "the system random source failed".to_string())?;
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    };
    let path = dir.join(TOKEN);
    let written = token.clone();
    // blocking: off the runtime's thread.
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path).map_err(|e| format!("{} cannot be written: {e}", path.display()))?;
        f.write_all(written.as_bytes()).map_err(|e| format!("{} cannot be written: {e}", path.display()))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok((listener, token))
}

pub(super) async fn accept(listener: TcpListener, state: Shared) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::task::spawn_local(connection(stream, state.clone()));
            }
            Err(e) => {
                eprintln!("ws accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

type Sink = futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>;

async fn connection(stream: TcpStream, state: Shared) {
    let _ = stream.set_nodelay(true);
    let config = WebSocketConfig::default().max_message_size(Some(MAX_IN)).max_frame_size(Some(MAX_IN));
    let Ok(Ok(ws)) = tokio::time::timeout(HELLO_WAIT, tokio_tungstenite::accept_async_with_config(stream, Some(config))).await else {
        return;
    };
    let (sink, mut incoming) = ws.split();
    let sink = Rc::new(Mutex::new(sink));
    let hello = match tokio::time::timeout(HELLO_WAIT, incoming.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => client_frame(&b),
        _ => None,
    };
    let token = state.borrow().websocket.as_ref().map(|(_, t)| t.clone());
    let (id, outbox) = match server::welcome(&state, hello, Some(token.as_deref().unwrap_or_default())) {
        Ok(c) => c,
        Err(refused) => {
            let mut s = sink.lock().await;
            let _ = s.send(Message::Binary(batch("", 0, &server::encode(&refused)).encode().into())).await;
            let _ = s.close().await;
            return;
        }
    };
    let heartbeat = state.borrow().opts.heartbeat;
    let acked = Rc::new(Cell::new(0u64));
    let acks = Rc::new(Notify::new());
    let heard = Rc::new(Cell::new(Instant::now()));
    let writer = tokio::task::spawn_local(write(state.clone(), id, outbox.clone(), sink.clone(), acked.clone(), acks.clone()));
    // Its own task: a batch waiting on a window, a host busy with a tool
    // or a session typing flat out never holds a ping back.
    // Rung when nothing has been heard for three beats: the peer is gone,
    // whatever TCP has yet to notice.
    let dead = Rc::new(Notify::new());
    let pinger = {
        let (sink, heard, dead) = (sink.clone(), heard.clone(), dead.clone());
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
            loop {
                tick.tick().await;
                // Asked before the sink is: a writer stuck on a full
                // socket holds it.
                if heard.get().elapsed() > heartbeat * 3 {
                    dead.notify_one();
                    return;
                }
                if sink.lock().await.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
        })
    };
    let mut gone = false;
    loop {
        let msg = tokio::select! {
            m = incoming.next() => match m {
                Some(Ok(m)) => m,
                _ => break,
            },
            _ = dead.notified() => {
                eprintln!("ws client {id}: nothing heard for {:?}: closing", heartbeat * 3);
                gone = true;
                break;
            }
        };
        heard.set(Instant::now());
        match msg {
            Message::Binary(b) => match Envelope::decode(&b) {
                Ok(e) if e.kind == KIND_ACK => {
                    acked.set(acked.get().max(e.seq));
                    acks.notify_one();
                }
                Ok(e) if e.kind == KIND_FRAME => match e.plain(MAX_IN).ok().and_then(|p| serde_json::from_slice::<ClientFrame>(&p).ok()) {
                    Some(f) => server::dispatch(&state, id, f),
                    None => eprintln!("ws client {id}: a frame that is no ClientFrame"),
                },
                Ok(e) => eprintln!("ws client {id}: a frame of kind {} it may not send", e.kind),
                Err(e) => eprintln!("ws client {id}: {e}"),
            },
            // tungstenite queues the pong as it reads the ping; flushed now,
            // not with the next batch.
            Message::Ping(_) => {
                let _ = sink.lock().await.flush().await;
            }
            Message::Close(_) => break,
            // A pong only says it is there, which `heard` has noted.
            _ => {}
        }
    }
    pinger.abort();
    server::gone(&state, id);
    acks.notify_one();
    if gone {
        // Its writer may be stuck on a socket nobody reads: dropped with
        // it, which closes the connection.
        writer.abort();
    }
    let _ = writer.await;
}

/// The one ClientFrame a hello message carries.
fn client_frame(b: &[u8]) -> Option<ClientFrame> {
    let e = Envelope::decode(b).ok().filter(|e| e.kind == KIND_FRAME)?;
    serde_json::from_slice(&e.plain(MAX_IN).ok()?).ok()
}

/// Hands the outbox on in batches, by time or by size, never more than
/// `WINDOW` ahead of the client's acks.
async fn write(state: Shared, id: u64, outbox: Rc<Outbox>, sink: Rc<Mutex<Sink>>, acked: Rc<Cell<u64>>, acks: Rc<Notify>) {
    let mut sent = 0u64;
    let mut buf = Vec::with_capacity(BATCH_BYTES);
    while outbox.ready().await {
        for (session, cursor) in outbox.due() {
            server::resync(&state, id, session, cursor);
        }
        if outbox.bytes() < BATCH_BYTES {
            tokio::time::sleep(BATCH_WINDOW).await;
        }
        for (session, outs) in outbox.take(BATCH_BYTES) {
            while sent >= acked.get() + WINDOW {
                acks.notified().await;
                if !outbox.open() {
                    return;
                }
            }
            buf.clear();
            let mut seq = 0;
            for o in &outs {
                buf.extend_from_slice(&o.bytes);
                seq = seq.max(o.seq);
            }
            let msg = batch(&session, seq, &buf).encode();
            if sink.lock().await.send(Message::Binary(msg.into())).await.is_err() {
                return;
            }
            sent += 1;
        }
    }
    let _ = sink.lock().await.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-LAG-5: the envelope round-trips, and a batch is compressed before
    /// the (reserved) encryption slot.
    #[test]
    fn r_lag_5_the_envelope_round_trips_and_batches_are_compressed() {
        let session = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let mut frames = Vec::new();
        for i in 0..200 {
            frames.extend_from_slice(format!("{{\"type\":\"line\",\"line\":{{\"type\":\"item.delta\",\"sessionId\":\"{session}\",\"turnId\":\"0199a1b2-c3d4-7e5f-8a9b-000000000001\",\"itemId\":\"0199a1b2-c3d4-7e5f-8a9b-000000000002\",\"delta\":{{\"type\":\"text\",\"text\":\"word{i} \"}}}},\"session\":\"{session}\",\"seq\":{i}}}\n").as_bytes());
        }
        let e = batch(session, 199, &frames);
        assert_eq!(e.flags & FLAG_ZSTD, FLAG_ZSTD);
        assert_eq!(e.enc, ENC_NONE);
        assert!(e.payload.len() * 5 < frames.len(), "{} bytes of {}", e.payload.len(), frames.len());
        let wire = e.encode();
        assert_eq!(&wire[..4], &[V, KIND_BATCH, FLAG_ZSTD, ENC_NONE]);
        assert_eq!(&wire[4..6], &[0x01, 0x99]);
        let back = Envelope::decode(&wire).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.plain(1 << 20).unwrap(), frames);
        // Never past the limit, however small the message.
        assert!(back.plain(1024).unwrap_err().contains("past"));
        assert!(Envelope::decode(&wire[..10]).is_err());
        assert_eq!(uuid("not a uuid"), [0u8; 16]);
    }
}
