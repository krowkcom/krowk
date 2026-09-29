//! The idle-forget invariant (canon, engineering/relay.md → Tickets), held
//! with shortened timers: a relay forgets a channel's fence only after the
//! ticket lifetime, the skew and a margin, so a ticket issued before the
//! lease last moved has expired by the time the fence that refuses it is
//! gone. Against the reference relay in-process; `script/check.mjs
//! invariant` holds the hosted relay to the same with the same timers.
#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use krowk_client::e2e::{self, SigningKey, RELAY_ROLE_HOST};
use krowk_client::relay_ticket::{self, Ticket};
use krowk_harness::daemon::ws::{Envelope, ENC_NONE, KIND_RELAY};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const FIXTURE: &str = include_str!("fixtures/relay/roster.json");

fn control(v: &Value) -> Vec<u8> {
    Envelope { kind: KIND_RELAY, flags: 0, enc: ENC_NONE, session: [0; 16], seq: 0, payload: serde_json::to_vec(v).unwrap() }.encode()
}

/// A host join with `ticket`: the first answer after the challenge, or the
/// refusal at the upgrade. The socket is kept open for a joined host.
type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn host_join(url: &str, f: &Value, ticket: &str, fence: u64) -> (Value, Ws) {
    let s = &f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == "idle-host").unwrap();
    let sid = s["id"].as_str().unwrap();
    let mut req = format!("{url}/v1/relay/{sid}").into_client_request().unwrap();
    req.headers_mut().insert("x-krowk-ticket", ticket.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let next = async |ws: &mut Ws| loop {
        if let Some(Ok(Message::Binary(b))) = ws.next().await {
            let e = Envelope::decode(&b).unwrap();
            if e.kind == KIND_RELAY {
                return serde_json::from_slice::<Value>(&e.payload).unwrap();
            }
        }
    };
    let first = next(&mut ws).await;
    if first["type"] == "error" {
        return (first, ws);
    }
    let nonce: [u8; 32] = e2e::unhex(first["nonce"].as_str().unwrap()).unwrap().try_into().unwrap();
    let key = SigningKey::from_secret(&e2e::unhex(d["seed"].as_str().unwrap()).unwrap()).unwrap();
    let device = e2e::DeviceId::parse(d["id"].as_str().unwrap()).unwrap();
    let session = krowk_harness::daemon::ws::uuid(sid);
    let sig = key.sign_relay_join(RELAY_ROLE_HOST, &session, &nonce, &device, url).unwrap();
    ws.send(Message::Binary(control(&json!({"type": "join", "role": "host", "device": device.to_string(), "signature": e2e::hex(&sig), "fence": fence, "stream": "11".repeat(16)})).into())).await.unwrap();
    (next(&mut ws).await, ws)
}

/// R-RELAY-1: with a two-second ticket lifetime, one second of skew and one
/// of margin, a ticket from before the lease moved is refused
/// `not_lease_holder` while the channel holds its fence, and has expired
/// by the time the channel could forget it.
#[tokio::test]
async fn r_relay_1_a_channel_forgets_its_fence_only_after_every_older_ticket_expired() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let limits = krowk_harness::relay::Limits { ticket_lifetime: 2, ticket_skew: 1, idle_margin: 1, ..Default::default() };
    assert_eq!(limits.idle(), Duration::from_secs(4));
    let roster = krowk_harness::relay::Roster::parse(FIXTURE).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits }));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let s = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == "idle-host").unwrap();
    let fence = s["fence"].as_u64().unwrap();
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    let now = relay_ticket::now();
    let ticket = |fence: u64| {
        Ticket {
            kid,
            role: RELAY_ROLE_HOST,
            env: relay_ticket::ENV_PRODUCTION,
            session: krowk_harness::daemon::ws::uuid(s["id"].as_str().unwrap()),
            device: e2e::unhex(d["id"].as_str().unwrap()).unwrap().try_into().unwrap(),
            signing_key: e2e::unhex(d["signingKey"].as_str().unwrap()).unwrap().try_into().unwrap(),
            fence,
            iat: now,
            exp: now + 2,
            workspace: d["workspace"].as_str().unwrap().to_string(),
        }
        .sign(&seed)
    };
    let stale = ticket(fence - 1);
    let current = ticket(fence);
    let (joined, host) = host_join(&url, &f, &current, fence).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    let (early, _) = host_join(&url, &f, &stale, fence - 1).await;
    assert_eq!(early["code"], "not_lease_holder", "{early}");
    drop(host);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (late, _) = host_join(&url, &f, &stale, fence - 1).await;
    assert_eq!(late["code"], "ticket_expired", "{late}");
}
