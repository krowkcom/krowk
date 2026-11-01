//! R-MCP-10, R-MCP-11, R-MCP-12: calls carry the stored bearer, an expired
//! token or a 401 refreshes once (rotating refresh tokens, two krowks at
//! once), and a server without a usable sign-in answers with the exact
//! tool error.

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::mcp::{Mcp, Pool, Server, Status};
use krowk_harness::oauth::Stored;
use mock::{Mock, Reply, Seen};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const SIGN_IN: &str = "mobbin needs sign-in: run `krowk mcp login mobbin` or open /mcp";

struct Fake {
    mcp: Mock,
    auth: Mock,
    /// The access token the server takes, and the refresh token the
    /// authorization server takes (once: it rotates).
    valid: Arc<Mutex<(String, String)>>,
    n: Arc<Mutex<u32>>,
}

fn fake() -> Fake {
    let valid = Arc::new(Mutex::new(("at-1".to_string(), "rt-1".to_string())));
    let n = Arc::new(Mutex::new(1));
    let (v, count) = (valid.clone(), n.clone());
    let auth = mock::serve_seen(move |seen: &Seen, _| {
        let f: std::collections::HashMap<String, String> = url::form_urlencoded::parse(seen.raw.as_bytes()).into_owned().collect();
        let mut v = v.lock().unwrap();
        if seen.path != "/token" || f.get("refresh_token") != Some(&v.1) || !f.contains_key("resource") {
            return Reply::json(400, &json!({"error": "invalid_grant"}));
        }
        let mut c = count.lock().unwrap();
        *c += 1;
        *v = (format!("at-{c}"), format!("rt-{c}"));
        Reply::json(200, &json!({"access_token": v.0, "refresh_token": v.1, "expires_in": 3600}))
    });
    let v = valid.clone();
    let mcp = mock::serve_seen(move |seen: &Seen, _| {
        let ok = seen.header("authorization") == Some(&format!("Bearer {}", v.lock().unwrap().0));
        if !ok {
            return Reply::status(401, "no");
        }
        let id = seen.body["id"].clone();
        let result = match seen.body["method"].as_str() {
            Some("initialize") => json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "f", "version": "1"}}),
            Some("tools/list") => json!({"tools": [{"name": "echo", "description": "d", "inputSchema": {"type": "object"}}]}),
            Some("tools/call") => json!({"content": [{"type": "text", "text": "ok"}]}),
            _ => return Reply::status(202, ""),
        };
        Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
    });
    Fake { mcp, auth, valid, n }
}

fn sign_in(f: &Fake, name: &str, expires_at_ms: Option<i64>) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("krowk-mcp-tokens-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join("credentials.json");
    let s = Stored { issuer: "i".into(), client_id: "c".into(), token_endpoint: format!("{}/token", f.auth.url), access_token: "at-1".into(), refresh_token: Some("rt-1".into()), expires_at_ms, scope: "openid".into(), obtained_at_ms: 1 };
    krowk_harness::mcp_oauth::save(&path, "mobbin", &format!("{}/mcp", f.mcp.url), s).unwrap();
    path
}

fn server(f: &Fake) -> Server {
    Server { name: "mobbin".into(), config: serde_json::from_value(json!({"type": "http", "url": format!("{}/mcp", f.mcp.url)})).unwrap(), source: "test".into(), cwd: std::env::temp_dir() }
}

async fn call(mcp: &Mcp) -> (String, bool) {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    mcp.call(&json!({"tool": "mobbin:echo"}), &|_, _| false, &rx).await
}

async fn open(f: &Fake, path: &std::path::Path) -> (Pool, Mcp) {
    let pool = Pool::default().with_credentials(path.to_path_buf());
    let mcp = pool.adopt(vec![server(f)]).await;
    (pool, mcp)
}

fn stored_access(path: &std::path::Path) -> String {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["mcp"]["mobbin"]["accessToken"].as_str().unwrap().into()
}

#[tokio::test]
async fn r_mcp_10_a_call_carries_the_bearer_and_an_expired_token_or_a_401_refreshes_once() {
    let f = fake();
    let far = krowk_store::now_ms() + 3_600_000;
    let path = sign_in(&f, "bearer", Some(far));
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, ("ok".into(), false));
    assert_eq!(*f.n.lock().unwrap(), 1, "a fresh token is not refreshed");
    // The server revokes the token: the 401 refreshes once and the call is retried.
    f.valid.lock().unwrap().0 = "at-revoked".into();
    assert_eq!(call(&mcp).await, ("ok".into(), false));
    assert_eq!((*f.n.lock().unwrap(), stored_access(&path).as_str()), (2, "at-2"));
    pool.close().await;
    // Within a minute of expiry counts as expired.
    let path = sign_in(&f, "expiring", Some(krowk_store::now_ms() + 30_000));
    *f.valid.lock().unwrap() = ("at-1".into(), "rt-1".into());
    *f.n.lock().unwrap() = 1;
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, ("ok".into(), false));
    assert_eq!(*f.n.lock().unwrap(), 2);
    pool.close().await;
}

#[tokio::test]
async fn r_mcp_10_two_krowks_refreshing_a_rotating_token_at_once_both_end_with_a_valid_one() {
    let f = fake();
    let path = sign_in(&f, "race", Some(1));
    let ((p1, a), (p2, b)) = (open(&f, &path).await, open(&f, &path).await);
    let (x, y) = tokio::join!(call(&a), call(&b));
    assert_eq!((x, y), (("ok".into(), false), ("ok".into(), false)));
    assert_eq!(*f.n.lock().unwrap(), 2, "one refresh spent the token; the other krowk read its result");
    assert_eq!(stored_access(&path), "at-2");
    p1.close().await;
    p2.close().await;
}

#[tokio::test]
async fn r_mcp_10_11_12_no_tokens_or_a_failed_refresh_needs_sign_in_and_a_logout_closes_the_connection() {
    let f = fake();
    // No tokens at all.
    let path = std::env::temp_dir().join(format!("krowk-mcp-tokens-none-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, (SIGN_IN.into(), true));
    assert_eq!(pool.status().remove(0).status, Status::NeedsSignIn);
    pool.close().await;
    // A refresh token the authorization server refuses.
    let path = sign_in(&f, "refused", Some(1));
    f.valid.lock().unwrap().1 = "rt-other".into();
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, (SIGN_IN.into(), true));
    assert_eq!(pool.status().remove(0).status, Status::NeedsSignIn);
    pool.close().await;
    // Logout during a session.
    let path = sign_in(&f, "logout", Some(krowk_store::now_ms() + 3_600_000));
    *f.valid.lock().unwrap() = ("at-1".into(), "rt-1".into());
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, ("ok".into(), false));
    assert!(pool.sign_out("mobbin").await.unwrap());
    assert_eq!(pool.status().remove(0).status, Status::NeedsSignIn);
    assert_eq!(call(&mcp).await, (SIGN_IN.into(), true));
    assert!(krowk_harness::mcp_oauth::load(&path, "mobbin", &format!("{}/mcp", f.mcp.url)).unwrap().is_none());
    pool.close().await;
    // A logout from another krowk: this one notices on its next call.
    let path = sign_in(&f, "elsewhere", Some(krowk_store::now_ms() + 3_600_000));
    let (pool, mcp) = open(&f, &path).await;
    assert_eq!(call(&mcp).await, ("ok".into(), false));
    krowk_harness::mcp_oauth::remove(&path, "mobbin").unwrap();
    assert_eq!(call(&mcp).await, (SIGN_IN.into(), true));
    assert_eq!(pool.status().remove(0).status, Status::NeedsSignIn);
    pool.close().await;
}
