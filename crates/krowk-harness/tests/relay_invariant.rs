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
    join(url, f, "idle", "idle-host", RELAY_ROLE_HOST, ticket, fence).await
}

async fn join(url: &str, f: &Value, session: &str, device: &str, role: u8, ticket: &str, fence: u64) -> (Value, Ws) {
    let s = &f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == session).unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == device).unwrap();
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
    let sig = key.sign_relay_join(role, &session, &nonce, &device, url).unwrap();
    let join = if role == RELAY_ROLE_HOST {
        json!({"type": "join", "role": "host", "device": device.to_string(), "signature": e2e::hex(&sig), "fence": fence, "stream": "11".repeat(16)})
    } else {
        json!({"type": "join", "role": "viewer", "device": device.to_string(), "signature": e2e::hex(&sig)})
    };
    ws.send(Message::Binary(control(&join).into())).await.unwrap();
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
    std::thread::spawn(move || krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits, state: None }));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let s = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == "idle-host").unwrap();
    let fence = s["fence"].as_u64().unwrap();
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    let now = relay_ticket::now();
    let ticket_at = |now: u64, fence: u64| {
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
    let ticket = |fence: u64| ticket_at(now, fence);
    let stale = ticket(fence - 1);
    let current = ticket(fence);
    let (joined, host) = host_join(&url, &f, &current, fence).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    let (early, _) = host_join(&url, &f, &stale, fence - 1).await;
    assert_eq!(early["code"], "not_lease_holder", "{early}");
    drop(host);
    // A fresh ticket at the old fence pins the forget's timing: a second
    // before the idle time the fence still refuses it. (This relay forgets
    // lazily, when another channel is entered, so the forget itself is not
    // observed here; the hosted relay's check observes it.)
    tokio::time::sleep(Duration::from_secs(3)).await;
    let fresh = ticket_at(relay_ticket::now(), fence - 1);
    let (held, _) = host_join(&url, &f, &fresh, fence - 1).await;
    assert_eq!(held["code"], "not_lease_holder", "a second before the idle time: {held}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (late, _) = host_join(&url, &f, &stale, fence - 1).await;
    assert_eq!(late["code"], "ticket_expired", "{late}");
}

fn start(limits: krowk_harness::relay::Limits, state: Option<std::path::PathBuf>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let roster = krowk_harness::relay::Roster::parse(FIXTURE).unwrap();
    std::thread::spawn(move || krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits, state }));
    url
}

/// The fixture's host ticket for session `idle` at `fence`, issued at `iat`
/// on the registry's clock, living `life` seconds.
fn host_ticket(f: &Value, iat: u64, life: u64, fence: u64) -> String {
    let s = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == "idle-host").unwrap();
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    Ticket {
        kid,
        role: RELAY_ROLE_HOST,
        env: relay_ticket::ENV_PRODUCTION,
        session: krowk_harness::daemon::ws::uuid(s["id"].as_str().unwrap()),
        device: e2e::unhex(d["id"].as_str().unwrap()).unwrap().try_into().unwrap(),
        signing_key: e2e::unhex(d["signingKey"].as_str().unwrap()).unwrap().try_into().unwrap(),
        fence,
        iat,
        exp: iat + life,
        workspace: d["workspace"].as_str().unwrap().to_string(),
    }
    .sign(&seed)
}

/// R-RELAY-1: the invariant holds whatever the registry's clock reads
/// against the relay's, as long as it runs forward. With the registry 8 s
/// ahead (lifetime 3 s, skew 1 s, margin 1 s, so idle is 5 s), the tickets
/// are not yet valid until 7 s from now; the holder joins then and leaves;
/// polled every half second for 12 s after, a ticket from before the lease
/// moved on is never admitted: the channel keeps its fence until the
/// relay's clock has passed the fence ticket's expiry.
#[tokio::test]
async fn r_relay_1_the_fence_is_kept_whatever_the_registrys_clock_reads() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    let url = start(krowk_harness::relay::Limits { ticket_lifetime: 3, ticket_skew: 1, idle_margin: 1, ..Default::default() }, None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let fence = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap()["fence"].as_u64().unwrap();
    let registry_now = relay_ticket::now() + 8;
    let stale = host_ticket(&f, registry_now, 3, fence - 1);
    let current = host_ticket(&f, registry_now, 3, fence);
    let (early, _) = host_join(&url, &f, &current, fence).await;
    assert_eq!(early["code"], "ticket_expired", "not yet valid: {early}");
    tokio::time::sleep(Duration::from_millis(7200)).await;
    let (joined, host) = host_join(&url, &f, &current, fence).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    drop(host);
    for _ in 0..24 {
        // A join elsewhere is when this relay sweeps its idle channels, so
        // each poll gives it the chance to forget this one.
        let (_, _other) = join(&url, &f, "auth", "auth-viewer", e2e::RELAY_ROLE_VIEWER, &viewer_ticket(&f, "auth", "auth-viewer"), 0).await;
        let (answer, _) = host_join(&url, &f, &stale, fence - 1).await;
        assert_ne!(answer["type"], "joined", "a ticket from before the lease moved was admitted: {answer}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A viewer ticket for `device` on `session`, at the relay's clock.
fn viewer_ticket(f: &Value, session: &str, device: &str) -> String {
    let s = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == session).unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == device).unwrap();
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    let now = relay_ticket::now();
    Ticket {
        kid,
        role: e2e::RELAY_ROLE_VIEWER,
        env: relay_ticket::ENV_PRODUCTION,
        session: krowk_harness::daemon::ws::uuid(s["id"].as_str().unwrap()),
        device: e2e::unhex(d["id"].as_str().unwrap()).unwrap().try_into().unwrap(),
        signing_key: e2e::unhex(d["signingKey"].as_str().unwrap()).unwrap().try_into().unwrap(),
        fence: 0,
        iat: now,
        exp: now + 3,
        workspace: d["workspace"].as_str().unwrap().to_string(),
    }
    .sign(&seed)
}

/// R-RELAY-1: under `--state`, a channel's fence outlives the relay: a
/// second relay started on the same state refuses a displaced holder.
#[tokio::test]
async fn r_relay_1_a_restart_with_state_keeps_the_fence() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    let dir = std::env::temp_dir().join(format!("krowk-relay-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fence = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "idle").unwrap()["fence"].as_u64().unwrap();
    let now = relay_ticket::now();
    let first = start(Default::default(), Some(dir.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (joined, _host) = host_join(&first, &f, &host_ticket(&f, now, 300, fence), fence).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    let second = start(Default::default(), Some(dir.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (answer, _) = host_join(&second, &f, &host_ticket(&f, now, 300, fence - 1), fence - 1).await;
    assert_eq!(answer["code"], "not_lease_holder", "after the restart: {answer}");
    // And without state, the same restart would have forgotten it.
    let bare = start(Default::default(), None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (forgot, _) = host_join(&bare, &f, &host_ticket(&f, now, 300, fence - 1), fence - 1).await;
    assert_eq!(forgot["type"], "joined", "{forgot}");
    let _ = std::fs::remove_dir_all(&dir);
}
