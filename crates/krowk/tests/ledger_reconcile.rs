//! A request the client killed still ran, and still billed, on the provider's
//! side. This holds the whole path to that fact: a stand-in provider that
//! meters every request it finishes into its ledger — one its client waited
//! for, whose answer lands in a transcript, and one its client gave up on —
//! then `sessions import` and the listing, run as the binary does.
//!
//! The binary runs with an empty environment and a temporary HOME, so the
//! store and the ledger directory are the test's own.

#![cfg(feature = "sessions")]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[test]
fn a_request_killed_client_side_is_still_counted_from_the_provider_ledger() {
    let home = scratch();
    let ledger = home.join(".krowk/ledger/fake.jsonl");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();

    // The provider: it reads a request, generates for `think` ms, meters
    // the completion — as a provider does whether or not anybody is still
    // listening — and answers, into a closed socket if need be.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let meter = ledger.clone();
    let provider = std::thread::spawn(move || {
        for (id, think, output) in [("gen_seen", 0, 1379), ("gen_ghost", 400, 3422)] {
            let (mut conn, _) = listener.accept().unwrap();
            let body = read_body(&mut conn);
            let model = body["model"].as_str().unwrap();
            std::thread::sleep(Duration::from_millis(think));
            let row = serde_json::json!({ "id": id, "provider": "fake", "model": model, "input_tokens": 84, "output_tokens": output, "reasoning_tokens": output - 17, "cost_usd": output as f64 * 1.2e-6 });
            std::fs::OpenOptions::new().create(true).append(true).open(&meter).unwrap().write_all(format!("{row}\n").as_bytes()).unwrap();
            let answer = serde_json::json!({ "id": id, "model": model, "usage": { "prompt_tokens": 84, "completion_tokens": output } }).to_string();
            let _ = write!(conn, "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}", answer.len());
        }
    });
    let ask = |timeout: Duration| -> Result<Value, std::io::Error> {
        let mut client = TcpStream::connect(addr).unwrap();
        let req = r#"{"model":"qwen3.5-plus","max_tokens":1200,"messages":[{"role":"user","content":"hi"}]}"#;
        client.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\n\r\n{req}", req.len()).as_bytes()).unwrap();
        client.set_read_timeout(Some(timeout)).unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer)?;
        Ok(serde_json::from_str(answer.split("\r\n\r\n").nth(1).unwrap()).unwrap())
    };

    // The first call is answered, and the agent's transcript records it.
    let seen = ask(Duration::from_secs(5)).unwrap();
    transcript(&home, &seen);
    // The second the client gives up on after 100 ms, and goes away.
    let err = ask(Duration::from_millis(100)).expect_err("the client times out before the provider answers");
    assert!(matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut), "{err}");
    provider.join().unwrap();
    assert_eq!(std::fs::read_to_string(&ledger).unwrap().lines().count(), 2, "the provider metered both");

    let imported = krowk(&home, &["sessions", "import", "--from", "all"]);
    assert_eq!(imported["data"]["ledger"], serde_json::json!({ "observed": 1, "unobserved": 1, "duplicate": 0 }), "{imported}");

    let listed = krowk(&home, &["sessions", "--harness", "ledger"]);
    let sessions = listed["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["turns"].as_i64(), Some(2));

    let id = sessions[0]["id"].as_str().unwrap();
    let shown = krowk(&home, &["sessions", "show", id]);
    let turns = &shown["data"]["turns"];
    assert_eq!((turns[0]["status"].as_str(), turns[0]["total_tokens"].as_i64()), (Some("observed"), Some(0)), "the transcript counts it: {shown}");
    assert_eq!(turns[1]["status"], "unobserved", "{shown}");
    assert_eq!((turns[1]["input_tokens"].as_i64(), turns[1]["output_tokens"].as_i64()), (Some(84), Some(17)), "reasoning is split out of output");
    assert_eq!(turns[1]["cost_usd_micros"], 4106, "the provider's stated cost is kept");

    // With the ghost gone, every row is in a transcript: the ledger session
    // costs nothing here, and says where its cost was counted.
    std::fs::write(&ledger, std::fs::read_to_string(&ledger).unwrap().lines().next().unwrap().to_string() + "\n").unwrap();
    let solo = home.join(".krowk/ledger/solo.jsonl");
    std::fs::copy(&ledger, &solo).unwrap();
    std::fs::remove_file(&ledger).unwrap();
    krowk(&home, &["sessions", "import", "--from", "all"]);
    let listed = krowk(&home, &["sessions", "--harness", "ledger"]);
    let solo_row = listed["data"]["sessions"].as_array().unwrap().iter().find(|s| s["title"].as_str().unwrap().contains("(solo)")).unwrap().clone();
    assert_eq!((solo_row["cost_display"].as_str(), solo_row["cost_counted_elsewhere"].as_bool()), (Some("counted elsewhere"), Some(true)), "{listed}");
    let shown = krowk(&home, &["sessions", "show", solo_row["id"].as_str().unwrap()]);
    assert_eq!((shown["data"]["cost_display"].as_str(), shown["data"]["cost_usd"].as_f64()), (Some("counted elsewhere"), Some(0.0)));

    // A second import converges on the same rows.
    let again = krowk(&home, &["sessions", "import", "--from", "all"]);
    let ledger_row = again["data"]["providers"].as_array().unwrap().iter().find(|p| p["provider"] == "ledger").unwrap();
    assert_eq!(ledger_row["messages_inserted"], 0);
    assert_eq!(again["data"]["ledger"]["unobserved"], 1);
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// The JSON body of one HTTP message, read until it is all there.
fn read_body(conn: &mut TcpStream) -> Value {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).unwrap();
        assert!(n > 0, "the request ended early");
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw);
        if let Some((_, body)) = text.split_once("\r\n\r\n")
            && let Ok(v) = serde_json::from_str(body)
        {
            return v;
        }
    }
}

/// A Claude Code transcript of one answered call, as the agent writes it.
fn transcript(home: &Path, answer: &Value) {
    let dir = home.join(".claude/projects/-work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(home.join("work")).unwrap();
    let cwd = home.join("work").display().to_string();
    let sid = "33333333-3333-4333-8333-333333333333";
    let user = serde_json::json!({ "type": "user", "uuid": "u1", "sessionId": sid, "cwd": cwd, "timestamp": "2026-09-10T11:30:00Z", "message": { "role": "user", "content": "hi" } });
    let asst = serde_json::json!({
        "type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": sid, "cwd": cwd, "timestamp": "2026-09-10T11:30:02Z",
        "message": { "id": answer["id"], "role": "assistant", "model": answer["model"], "content": [{ "type": "text", "text": "hello" }],
            "usage": { "input_tokens": answer["usage"]["prompt_tokens"], "output_tokens": answer["usage"]["completion_tokens"] } }
    });
    std::fs::write(dir.join(format!("{sid}.jsonl")), format!("{user}\n{asst}\n")).unwrap();
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-ledger-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    home.canonicalize().unwrap()
}

fn krowk(home: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(args)
        .arg("--json")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .current_dir(home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {stdout}"))
}
