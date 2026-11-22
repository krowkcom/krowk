//! Costs in `sessions` and `sessions show`, priced per turn by the model that
//! turn ran on, against the 2026-09-10 OpenCode Zen demo: three models, three
//! calls each, and the ghost of a fourth qwen call the client killed. The
//! figures are the hand-verified ones; an unknown model reads —, never $0.
//!
//! The binary runs with an empty environment and a temporary HOME holding the
//! ledger and a price cache with the three Zen rates as models.dev had them.

#![cfg(feature = "sessions")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

const ZEN: &str = r#"{"id":"zen-01","provider":"opencode","model":"deepseek-v4-flash","input_tokens":18,"output_tokens":77}
{"id":"zen-02","provider":"opencode","model":"deepseek-v4-flash","input_tokens":109,"output_tokens":156,"reasoning_tokens":138}
{"id":"zen-03","provider":"opencode","model":"deepseek-v4-flash","input_tokens":30,"cache_read_tokens":108,"output_tokens":118,"reasoning_tokens":96}
{"id":"zen-04","provider":"opencode","model":"glm-5.3-flash","input_tokens":26,"output_tokens":1854,"reasoning_tokens":1849}
{"id":"zen-05","provider":"opencode","model":"glm-5.3-flash","input_tokens":44,"output_tokens":288}
{"id":"zen-06","provider":"opencode","model":"glm-5.3-flash","input_tokens":60,"output_tokens":209}
{"id":"zen-07","provider":"opencode","model":"qwen3.5-plus","input_tokens":24,"output_tokens":1580,"reasoning_tokens":1564}
{"id":"zen-08","provider":"opencode","model":"qwen3.5-plus","input_tokens":56,"output_tokens":2366,"reasoning_tokens":2352}
{"id":"zen-09","provider":"opencode","model":"qwen3.5-plus","input_tokens":84,"output_tokens":1379,"reasoning_tokens":1362}
{"id":"zen-10","provider":"opencode","model":"qwen3.5-plus","input_tokens":84,"output_tokens":3422,"reasoning_tokens":3405,"cost_usd":0.0041}
"#;

const RATES: &str = r#"{"opencode":{"models":{
"deepseek-v4-flash":{"cost":{"input":0.14,"output":0.28,"cache_read":0.028}},
"glm-5.3-flash":{"cost":{"input":0.15,"output":0.5,"cache_read":0.03}},
"qwen3.5-plus":{"cost":{"input":0.2,"output":1.2,"cache_read":0.02,"cache_write":0.25}}}}}"#;

#[test]
fn the_zen_demo_prices_per_turn_and_an_unknown_model_reads_as_a_dash() {
    let home = scratch();
    let ledger = home.join(".krowk/ledger");
    std::fs::create_dir_all(&ledger).unwrap();
    std::fs::write(ledger.join("zen.jsonl"), ZEN).unwrap();
    std::fs::write(ledger.join("mystery.jsonl"), r#"{"id":"m-1","provider":"opencode","model":"mystery-model","input_tokens":5,"output_tokens":5,"cost_usd":0}"#.to_string() + "\n").unwrap();
    let cache = home.join(".krowk/cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("models.json"), RATES).unwrap();
    std::fs::write(cache.join("models.meta.json"), r#"{"etag":"","fetched_at_ms":1789031459675}"#).unwrap();

    let imported = krowk(&home, &["sessions", "import", "--from", "ledger"]);
    assert_eq!(imported["data"]["unpriced_models"], serde_json::json!(["opencode/mystery-model"]), "{imported}");

    let listed = krowk(&home, &["sessions"]);
    let sessions = listed["data"]["sessions"].as_array().unwrap();
    let by_title = |t: &str| sessions.iter().find(|s| s["title"].as_str().unwrap().contains(t)).unwrap().clone();
    let (zen, mystery) = (by_title("(zen)"), by_title("(mystery)"));
    // opencode writes cost 0 for a model it cannot price; a stated 0 is no price.
    assert_eq!((mystery["cost_usd"].clone(), mystery["cost_display"].as_str()), (Value::Null, Some("—")), "a missing price is missing, not free");
    assert_eq!(mystery["unpriced"], serde_json::json!(["opencode/mystery-model"]));
    assert_eq!(listed["data"]["priced_with"], "priced at current rates (models.dev prices fetched 2026-09-10), not the rates in force at the time");
    close(zen["cost_usd"].as_f64().unwrap(), 0.000_123_284 + 0.001_195 + 0.006_422_8 + 0.0041);

    let shown = krowk(&home, &["sessions", "show", zen["id"].as_str().unwrap()]);
    let d = &shown["data"];
    let by_model = &d["cost_by_model"];
    close(by_model["opencode/deepseek-v4-flash"].as_f64().unwrap(), 0.000_123_284);
    close(by_model["opencode/glm-5.3-flash"].as_f64().unwrap(), 0.001_195);
    close(by_model["opencode/qwen3.5-plus"].as_f64().unwrap(), 0.006_422_8 + 0.0041);
    let turns = d["turns"].as_array().unwrap();
    // Unrounded all the way to the JSON: 18 × $0.14/M + 77 × $0.28/M.
    close(turns[0]["cost_usd"].as_f64().unwrap(), 0.000_024_08);
    assert_eq!((turns[0]["model"].as_str(), turns[0]["cost_source"].as_str()), (Some("deepseek-v4-flash"), Some("priced")));
    assert_eq!((turns[9]["cost_usd"].as_f64(), turns[9]["cost_source"].as_str()), (Some(0.0041), Some("reported")), "the provider's own figure wins");
    assert_eq!(turns[2]["reasoning_tokens"], 96, "the reasoning split survives into show");

    let human = run(&home, &["sessions", "show", zen["id"].as_str().unwrap(), "--format", "human"]);
    for want in ["turn 0  unobserved  95 tokens  $0.0000241", "glm-5.3-flash  1880 tokens  $0.000931", "3506 tokens  $0.00410 reported", "by model  opencode/deepseek-v4-flash $0.000123", "models.dev prices fetched 2026-09-10"] {
        assert!(human.contains(want), "missing {want:?} in:\n{human}");
    }
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

fn close(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-12, "got {got}, want {want}");
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("krowk-price-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    home.canonicalize().unwrap()
}

fn run(home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_krowk"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("KROWK_NO_UPDATE_CHECK", "1")
        .current_dir(home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
    stdout
}

fn krowk(home: &Path, args: &[&str]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let stdout = run(home, &all);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {stdout}"))
}
