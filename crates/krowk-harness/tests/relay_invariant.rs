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
    join_after(url, f, session, device, role, ticket, fence, Duration::ZERO).await
}

#[allow(clippy::too_many_arguments)]
async fn join_after(url: &str, f: &Value, session: &str, device: &str, role: u8, ticket: &str, fence: u64, delay: Duration) -> (Value, Ws) {
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
    tokio::time::sleep(delay).await;
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
    // The restart: the first relay holds its state's lock while it runs,
    // so the second reads a copy of what the first persisted.
    let restarted = dir.with_extension("restarted");
    let _ = std::fs::remove_dir_all(&restarted);
    std::fs::create_dir_all(&restarted).unwrap();
    std::fs::copy(dir.join("relay-channels.jsonl"), restarted.join("relay-channels.jsonl")).unwrap();
    let second = start(Default::default(), Some(restarted.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (answer, _) = host_join(&second, &f, &host_ticket(&f, now, 300, fence - 1), fence - 1).await;
    assert_eq!(answer["code"], "not_lease_holder", "after the restart: {answer}");
    // And without state, the same restart would have forgotten it.
    let bare = start(Default::default(), None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (forgot, _) = host_join(&bare, &f, &host_ticket(&f, now, 300, fence - 1), fence - 1).await;
    assert_eq!(forgot["type"], "joined", "{forgot}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&restarted);
}

/// Starts a relay on `dir` and answers why it would not, or None if it did.
fn refused_to_start(dir: &std::path::Path) -> Option<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let roster = krowk_harness::relay::Roster::parse(FIXTURE).unwrap();
    let dir = dir.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = krowk_harness::relay::run(listener, krowk_harness::relay::Config { roster, origin: None, limits: Default::default(), state: Some(dir) });
        let _ = tx.send(r.err());
    });
    rx.recv_timeout(Duration::from_millis(500)).ok().flatten()
}

/// A state directory holding one good record, the fence a displaced holder
/// must meet, and `damage` done to it.
fn damaged(name: &str, damage: impl FnOnce(&std::path::Path)) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-relay-damaged-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let good = format!("{}\n", json!({"env": "production", "session": "00".repeat(16), "workspace": "ws_a", "fence": 241, "fenceExp": relay_ticket::now() + 300, "fenceDevice": "11".repeat(16)}));
    std::fs::write(dir.join("relay-channels.jsonl"), &good).unwrap();
    damage(&dir.join("relay-channels.jsonl"));
    dir
}

/// R-RELAY-1: a relay never starts on state it cannot trust, since starting
/// as though it knew no fence would let a displaced holder host: a
/// truncated record, a byte that is not UTF-8, NUL bytes, a record with no
/// fence, a file it may not read, and a rewrite cut off, each refuse, with
/// what to do.
#[test]
fn r_relay_1_a_relay_refuses_to_start_on_state_it_cannot_trust() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let append = |bytes: Vec<u8>| move |p: &std::path::Path| std::fs::OpenOptions::new().append(true).open(p).unwrap().write_all(&bytes).unwrap();
    type Damage = Box<dyn FnOnce(&std::path::Path)>;
    let cases: Vec<(&str, Damage)> = vec![
        ("truncated", Box::new(|p: &std::path::Path| {
            let b = std::fs::read(p).unwrap();
            std::fs::write(p, &b[..60]).unwrap();
        })),
        ("not-utf8", Box::new(append(b"\xff\n".to_vec()))),
        ("nul", Box::new(append(vec![0u8; 131]))),
        ("fence-null", Box::new(append(format!("{}\n", json!({"env": "production", "session": "00".repeat(16), "workspace": "ws_a", "fence": null, "fenceExp": 1})).into_bytes()))),
        ("unreadable", Box::new(|p: &std::path::Path| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap())),
        ("mid-compaction", Box::new(|p: &std::path::Path| std::fs::write(p.with_extension("jsonl.tmp"), b"{\"env\"").unwrap())),
    ];
    // A root test runner reads a 000 file anyway; the case is then moot.
    let root = unsafe { libc::geteuid() } == 0;
    for (name, damage) in cases {
        let dir = damaged(name, damage);
        if name == "unreadable" && root {
            continue;
        }
        let why = refused_to_start(&dir).unwrap_or_else(|| panic!("a relay started on {name} state"));
        assert!(why.starts_with("--state") && why.contains(" — "), "{name}: {why}");
        let _ = std::fs::set_permissions(dir.join("relay-channels.jsonl"), std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// R-RELAY-1: the state is private to the relay and to one relay: the
/// directory is 0700 and the file 0600, and a second relay on the same
/// directory refuses to start while the first holds it. Records whose
/// fences have long expired are pruned when the relay starts.
#[tokio::test]
async fn r_relay_1_a_relays_state_is_private_to_it_and_pruned() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = damaged("private", |p: &std::path::Path| {
        use std::io::Write as _;
        let old = json!({"env": "production", "session": "22".repeat(16), "workspace": "ws_a", "fence": 3, "fenceExp": 1000, "fenceDevice": null});
        std::fs::OpenOptions::new().append(true).open(p).unwrap().write_all(format!("{old}\n").as_bytes()).unwrap();
    });
    let _url = start(Default::default(), Some(dir.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("relay-channels.jsonl")).unwrap().permissions().mode() & 0o777, 0o600);
    let kept = std::fs::read_to_string(dir.join("relay-channels.jsonl")).unwrap();
    assert!(kept.contains(&"00".repeat(16)) && !kept.contains(&"22".repeat(16)), "the expired record is pruned, the live one kept: {kept}");
    let why = refused_to_start(&dir).expect("a second relay on held state");
    assert!(why.contains("another relay holds it"), "{why}");
}


/// A ticket for a fixture device on a fixture session, in `role`.
fn fixture_ticket(f: &Value, session: &str, device: &str, role: u8) -> (String, u64) {
    let s = f["sessions"].as_array().unwrap().iter().find(|s| s["name"] == session).unwrap();
    let d = f["devices"].as_array().unwrap().iter().find(|d| d["name"] == device).unwrap();
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    let fence = if role == RELAY_ROLE_HOST { s["fence"].as_u64().unwrap() } else { 0 };
    let now = relay_ticket::now();
    let t = Ticket {
        kid,
        role,
        env: relay_ticket::ENV_PRODUCTION,
        session: krowk_harness::daemon::ws::uuid(s["id"].as_str().unwrap()),
        device: e2e::unhex(d["id"].as_str().unwrap()).unwrap().try_into().unwrap(),
        signing_key: e2e::unhex(d["signingKey"].as_str().unwrap()).unwrap().try_into().unwrap(),
        fence,
        iat: now,
        exp: now + 300,
        workspace: d["workspace"].as_str().unwrap().to_string(),
    };
    (t.sign(&seed), fence)
}

/// R-RELAY-1: a workspace spreading its own tickets one to a session evicts
/// only itself. The reference relay's ticketed pool at a tenth of its size
/// (128, and 16 a workspace): one pending connection on each of 128 of a
/// tenant's sessions, then 150 more on fresh ones, while another
/// workspace's host and two viewers answer in 600 ms — all three join,
/// three times. The same at full size is `relay_conformance.rs`'s heavy
/// test.
#[tokio::test]
async fn r_relay_1_a_workspace_spread_over_its_own_sessions_evicts_only_itself() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    let url = start(krowk_harness::relay::Limits { unjoined_ticketed: 128, unjoined_per_workspace: 16, ..Default::default() }, None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seed: [u8; 32] = e2e::unhex(f["ticketSeed"].as_str().unwrap()).unwrap().try_into().unwrap();
    let kid: [u8; 8] = e2e::unhex(f["ticketKeys"].as_object().unwrap().keys().next().unwrap()).unwrap().try_into().unwrap();
    let spreader = SigningKey::generate();
    let now = relay_ticket::now();
    let tenant = |i: u32| {
        let session = format!("{:08x}-0000-7000-8000-{:012x}", i + 1, 0x5b3);
        let t = Ticket {
            kid,
            role: e2e::RELAY_ROLE_VIEWER,
            env: relay_ticket::ENV_PRODUCTION,
            session: krowk_harness::daemon::ws::uuid(&session),
            device: [0x5b; 16],
            signing_key: spreader.public().0,
            fence: 0,
            iat: now,
            exp: now + 300,
            workspace: "ws_spreader".into(),
        };
        (session, t.sign(&seed))
    };
    let flood = |tickets: Vec<(String, String)>, url: String| {
        futures_util::future::join_all(tickets.into_iter().enumerate().map(move |(n, (session, ticket))| {
            let url = url.clone();
            tokio::spawn(async move {
                let mut req = format!("{url}/v1/relay/{session}").into_client_request().unwrap();
                req.headers_mut().insert("x-krowk-ticket", ticket.parse().unwrap());
                let target: std::net::SocketAddr = url.trim_start_matches("ws://").parse().ok()?;
                let sock = tokio::net::TcpSocket::new_v4().ok()?;
                sock.bind(format!("127.{}.{}.1:0", 60 + n % 20, n / 20 + 1).parse().unwrap()).ok()?;
                let stream = sock.connect(target).await.ok()?;
                tokio_tungstenite::client_async(req, tokio_tungstenite::MaybeTlsStream::Plain(stream)).await.ok().map(|(ws, _)| ws)
            })
        }))
    };
    let mut held = flood((0..128).map(tenant).collect(), url.clone()).await;
    for round in 0..3u32 {
        let fresh: Vec<_> = (0..150).map(|i| tenant(1000 + round * 150 + i)).collect();
        let u = url.clone();
        let burst = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            flood(fresh, u).await
        });
        let wait = Duration::from_millis(600);
        let (th, fh) = fixture_ticket(&f, "pool3", "pool3-host", RELAY_ROLE_HOST);
        let (tv1, _) = fixture_ticket(&f, "pool3", "pool3-viewer", e2e::RELAY_ROLE_VIEWER);
        let (tv2, _) = fixture_ticket(&f, "pool3", "pool3-viewer2", e2e::RELAY_ROLE_VIEWER);
        let ((h, _hw), (v1, _w1), (v2, _w2)) = tokio::join!(
            join_after(&url, &f, "pool3", "pool3-host", RELAY_ROLE_HOST, &th, fh, wait),
            join_after(&url, &f, "pool3", "pool3-viewer", e2e::RELAY_ROLE_VIEWER, &tv1, 0, wait),
            join_after(&url, &f, "pool3", "pool3-viewer2", e2e::RELAY_ROLE_VIEWER, &tv2, 0, wait),
        );
        for (who, answer) in [("host", h), ("viewer", v1), ("viewer2", v2)] {
            assert_eq!(answer["type"], "joined", "round {round}: {who}: {answer}");
        }
        held.extend(burst.await.unwrap());
    }
}
