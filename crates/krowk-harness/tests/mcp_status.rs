//! R-MCP-3, R-MCP-4: a server's status, the retry of a failed one, and
//! `reconnect`, against the stand-in stdio server (`fixtures/mcp/fake-mcp`)
//! and a stand-in HTTP one.
#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::mcp::{Pool, Server, Status};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

fn server(name: &str, config: Value) -> Server {
    Server { name: name.into(), config: serde_json::from_value(config).unwrap(), source: "test".into(), cwd: std::env::temp_dir() }
}

fn fake(dir: &std::path::Path, delay: Option<&str>) -> Server {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/fake-mcp");
    let mut env = json!({"FAKE_LOG": dir.join("log"), "FAKE_DIE": dir.join("die")});
    if let Some(d) = delay {
        env["FAKE_DELAY"] = json!(d);
    }
    server("fake", json!({"command": "bash", "args": [script], "env": env}))
}

fn initializes(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default().matches("\"method\":\"initialize\"").count()
}

async fn call(mcp: &krowk_harness::mcp::Mcp) -> (String, bool) {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    mcp.call(&json!({"tool": "fake:echo", "input": {"text": "hi"}}), &|_, _| false, &rx).await
}

async fn status(pool: &Pool) -> Status {
    pool.status().remove(0).status
}

async fn until(pool: &Pool, want: Status) {
    for _ in 0..100 {
        if status(pool).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never {want:?}: {:?}", pool.status());
}

#[tokio::test]
async fn r_mcp_4_an_exited_server_is_failed_retried_after_the_interval_and_at_once_on_reconnect() {
    let dir = std::env::temp_dir().join(format!("krowk-mcp-status-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = Pool::with_retry(Duration::from_millis(1500));
    let mcp = pool.adopt(vec![fake(&dir, None)]).await;
    assert_eq!(status(&pool).await, Status::Connecting);
    assert_eq!(call(&mcp).await, ("echo: hi".into(), false));
    let s = pool.status().remove(0);
    assert_eq!((s.tool_count(), s.status), (1, Status::Connected));
    // The process exits mid-call: failed, with a reason; the next call fails fast.
    std::fs::write(dir.join("die"), "").unwrap();
    assert!(call(&mcp).await.1);
    let Status::Failed(why) = status(&pool).await else { panic!("{:?}", pool.status()) };
    assert!(!why.is_empty());
    let (out, err) = call(&mcp).await;
    assert!(err && out.contains("not running"), "{out}");
    assert_eq!(initializes(&dir), 1, "no retry inside the interval");
    // After the interval, a call starts it again.
    std::fs::remove_file(dir.join("die")).unwrap();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(call(&mcp).await, ("echo: hi".into(), false));
    assert_eq!(initializes(&dir), 2);
    // Reconnect does not wait for the interval.
    std::fs::write(dir.join("die"), "").unwrap();
    call(&mcp).await;
    std::fs::remove_file(dir.join("die")).unwrap();
    assert!(pool.reconnect("fake").await);
    assert!(!pool.reconnect("nope").await);
    until(&pool, Status::Connected).await;
    assert_eq!(initializes(&dir), 3);
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn r_mcp_3_a_server_still_connecting_is_waited_for_by_a_call_and_one_answering_401_needs_sign_in() {
    let dir = std::env::temp_dir().join(format!("krowk-mcp-status-slow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = Pool::default();
    let mcp = pool.adopt(vec![fake(&dir, Some("2"))]).await;
    let t = std::time::Instant::now();
    mcp.warm(&|_, _| false);
    assert!(t.elapsed() < Duration::from_millis(500), "warm does not wait");
    assert_eq!(status(&pool).await, Status::Connecting);
    assert_eq!(call(&mcp).await, ("echo: hi".into(), false));
    assert!(t.elapsed() >= Duration::from_secs(2), "the call waited for the start");
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);

    let web = mock::serve_seen(|_, _| mock::Reply::status(401, "sign in"));
    let pool = Pool::default();
    let mcp = pool.adopt(vec![server("web", json!({"type": "http", "url": web.url}))]).await;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    mcp.search(&json!({"query": "x"}), &|_, _| false, &rx).await;
    assert_eq!(status(&pool).await, Status::NeedsSignIn);
}
