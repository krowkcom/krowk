//! `sessions budget` as the binary answers it: a Claude turn sent with
//! `max_tokens: 1200` whose model thought for 3,405 tokens trips the guard on
//! what was metered; a turn written after the last import is counted without
//! a sync; a subagent's spend is its parent's; and dollars reach the
//! comparison unrounded through the store.

#![cfg(feature = "sessions")]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const SID: &str = "66666666-6666-4666-8666-666666666666";

#[test]
fn a_runaway_reasoning_run_trips_the_guard_on_metered_tokens() {
    let home = Scratch::new("runaway");
    let h = &home.0;
    let (dir, cwd) = project(h);
    let user = json!({ "type": "user", "uuid": "u1", "sessionId": SID, "cwd": cwd, "message": { "role": "user", "content": "one word: what is zen?" } });
    let asst = assistant("a1", "msg_1", 3422, 3405);
    std::fs::write(dir.join(format!("{SID}.jsonl")), format!("{user}\n{asst}\n")).unwrap();
    // Never imported: the first check in a fresh session finds it by the
    // agent's own id instead of failing with no_session.
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-tokens", "100000", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["data"]["refreshed"], "imported", "{out}");

    // The cap it was sent with (1200) is inside 2,000; what was generated is not.
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-tokens", "2000", "--json"]);
    assert_eq!(code, 4, "{out}");
    let err: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(err["error"]["error"], "budget_exceeded", "{err}");
    let details = &err["error"]["details"];
    let m = &details["metered"];
    assert_eq!((m["output"].as_i64(), m["reasoning"].as_i64(), m["generated"].as_i64()), (Some(17), Some(3405), Some(3422)), "{err}");
    assert_eq!(details["tripped"][0], json!({ "limit": "max_tokens", "generated": 3422, "max": 2000 }));
    assert!(err["error"]["fix"].as_str().unwrap().contains("stop the run"), "{err}");

    // Priced from the embedded snapshot: 84 × $3/M + 3,422 × $15/M.
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-usd", "0.06", "--max-tokens", "4000", "--json"]);
    assert_eq!(code, 0, "{out}");
    let ok: Value = serde_json::from_str(&out).unwrap();
    assert!((ok["data"]["cost_usd"].as_f64().unwrap() - (84.0 * 3.0 + 3422.0 * 15.0) / 1e6).abs() < 1e-12, "{ok}");
    assert_eq!(run(h, &["sessions", "budget", SID, "--max-usd", "0.05"]).0, 4, "$0.051582 is over $0.05");

    // Written after the last import, and counted without a sync.
    let more = format!("{}\n", assistant("a2", "msg_2", 5000, 0));
    std::fs::OpenOptions::new().append(true).open(dir.join(format!("{SID}.jsonl"))).unwrap().write_all(more.as_bytes()).unwrap();
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-tokens", "8000", "--json"]);
    assert_eq!(code, 4, "the new turn is metered: {out}");

    // A check that has to write waits out an import holding the store, then
    // answers current — it never passes on what was there before.
    let more = format!("{}\n", assistant("a3", "msg_3", 100, 0));
    std::fs::OpenOptions::new().append(true).open(dir.join(format!("{SID}.jsonl"))).unwrap().write_all(more.as_bytes()).unwrap();
    let lock = std::fs::OpenOptions::new().read(true).write(true).open(h.join(".krowk/sessions/import.lock")).unwrap();
    lock.try_lock().unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        drop(lock);
    });
    let started = std::time::Instant::now();
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-tokens", "100000", "--json"]);
    holder.join().unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(700), "it waited for the lock");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["data"]["metered"]["generated"], 3422 + 5000 + 100, "{out}");

    // A subagent's spend is its parent's.
    let agent = dir.join(SID).join("subagents");
    std::fs::create_dir_all(&agent).unwrap();
    let sub = json!({ "type": "assistant", "uuid": "s1", "sessionId": SID, "agentId": "a77", "isSidechain": true, "cwd": cwd,
        "message": { "id": "msg_s1", "role": "assistant", "model": "claude-sonnet-4-6", "content": [{ "type": "text", "text": "done" }], "usage": { "input_tokens": 1, "output_tokens": 600 } } });
    std::fs::write(agent.join("agent-a77.jsonl"), format!("{sub}\n")).unwrap();
    let (code, out) = run(h, &["sessions", "budget", SID, "--max-tokens", "9100", "--json"]);
    assert_eq!(code, 4, "{out}");
    let err: Value = serde_json::from_str(&out).unwrap();
    assert_eq!((err["error"]["details"]["subagents"].as_i64(), err["error"]["details"]["metered"]["generated"].as_i64()), (Some(1), Some(3422 + 5000 + 100 + 600)), "{err}");

    // A limit given blank is a mistake, not a switched-off check; none at all too.
    assert_eq!(run(h, &["sessions", "budget", SID, "--max-usd", "", "--max-tokens", "1"]).0, 1);
    assert_eq!(run(h, &["sessions", "budget", SID]).0, 1);
}

#[test]
fn dollars_reach_the_guard_unrounded_through_the_store() {
    let home = Scratch::new("precision");
    let h = &home.0;
    let ledger = h.join(".krowk/ledger");
    std::fs::create_dir_all(&ledger).unwrap();
    // Three Zen deepseek calls, priced from their tokens: 24.08 + 58.94 + 40.264 µ$.
    std::fs::write(
        ledger.join("zen.jsonl"),
        [
            r#"{"id":"z1","provider":"opencode","model":"deepseek-v4-flash","input_tokens":18,"output_tokens":77}"#,
            r#"{"id":"z2","provider":"opencode","model":"deepseek-v4-flash","input_tokens":109,"output_tokens":156,"reasoning_tokens":138}"#,
            r#"{"id":"z3","provider":"opencode","model":"deepseek-v4-flash","input_tokens":30,"cache_read_tokens":108,"output_tokens":118,"reasoning_tokens":96}"#,
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    let cache = h.join(".krowk/cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("models.json"), r#"{"opencode":{"models":{"deepseek-v4-flash":{"cost":{"input":0.14,"output":0.28,"cache_read":0.028}}}}}"#).unwrap();
    assert_eq!(run(h, &["sessions", "import", "--from", "ledger", "--json"]).0, 0);
    let (_, listed) = run(h, &["sessions", "--json"]);
    let id = serde_json::from_str::<Value>(&listed).unwrap()["data"]["sessions"][0]["id"].as_str().unwrap().to_string();
    // $0.000123284 is over $0.000123, though both read $0.0001 at four places.
    let (code, out) = run(h, &["sessions", "budget", &id, "--max-usd", "0.000123", "--json"]);
    assert_eq!(code, 4, "{out}");
    let err: Value = serde_json::from_str(&out).unwrap();
    let cost = err["error"]["details"]["cost_usd"].as_f64().unwrap();
    assert!((cost - (0.000_024_08 + 0.000_058_94 + 0.000_040_264)).abs() < 1e-15, "{cost}");
    assert_eq!(run(h, &["sessions", "budget", &id, "--max-usd", "0.000124"]).0, 0);
}

use std::io::Write;

fn assistant(uuid: &str, id: &str, output: i64, thinking: i64) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "sessionId": SID,
        "message": { "id": id, "role": "assistant", "model": "claude-sonnet-4-6", "content": [{ "type": "text", "text": "Sitting." }],
            "usage": { "input_tokens": 84, "output_tokens": output, "output_tokens_details": { "thinking_tokens": thinking } } }
    })
}

fn project(home: &Path) -> (PathBuf, String) {
    let dir = home.join(".claude/projects/-work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(home.join("work")).unwrap();
    (dir, home.join("work").display().to_string())
}

/// A temporary HOME, removed however the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("krowk-budget-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(home: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .current_dir(home)
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}
