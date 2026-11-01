//! `krowk mcp add|remove|list`, the built binary: servers written to
//! config.json, refused when duplicated or badly named, listed from every
//! source without connecting, and removed only where krowk owns them.

#![cfg(feature = "harness")]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-mcp-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn krowk(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn list(&self) -> Vec<Value> {
        let out = self.krowk(&["mcp", "list", "--json"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["data"]["servers"].as_array().unwrap().clone()
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn add_list_and_remove_round_trip_through_config_json() {
    let s = Sandbox::new("round");
    assert!(s.krowk(&["mcp", "add", "mobbin", "https://api.mobbin.com/mcp", "--header", "X-Key: ${KEY}", "--header", "A: b"]).status.success());
    let out = s.krowk(&["mcp", "add", "files", "--env", "K=V", "--", "npx", "-y", "srv", "--flag"]);
    assert!(out.status.success(), "{}", text(&out));
    let config: Value = serde_json::from_slice(&std::fs::read(s.root.join("home/.krowk/config.json")).unwrap()).unwrap();
    assert_eq!(config["mcpServers"]["mobbin"], json!({"type": "http", "url": "https://api.mobbin.com/mcp", "headers": {"X-Key": "${KEY}", "A": "b"}}));
    assert_eq!(config["mcpServers"]["files"], json!({"command": "npx", "args": ["-y", "srv", "--flag"], "env": {"K": "V"}}));

    let rows = s.list();
    let mobbin = rows.iter().find(|r| r["name"] == "mobbin").unwrap();
    assert_eq!((mobbin["transport"].as_str(), mobbin["source"].as_str()), (Some("http"), Some("krowk's config.json")));
    assert_eq!(rows.iter().find(|r| r["name"] == "files").unwrap()["transport"], "stdio");

    // A duplicate is refused, then replaced with --force; a bad name is refused with the rule.
    let dup = s.krowk(&["mcp", "add", "mobbin", "https://other.example/mcp"]);
    assert!(!dup.status.success() && text(&dup).contains("--force"), "{}", text(&dup));
    assert!(s.krowk(&["mcp", "add", "mobbin", "https://other.example/mcp", "--force"]).status.success());
    let bad = s.krowk(&["mcp", "add", "a__b", "https://x.example/mcp"]);
    assert!(!bad.status.success() && text(&bad).contains("no __"), "{}", text(&bad));

    assert_eq!(s.list().iter().find(|r| r["name"] == "mobbin").unwrap()["sign_in"], "not signed in");
    // A stored sign-in goes with it; the credentials file's other keys stay.
    let creds = s.root.join("home/.krowk/credentials.json");
    let entry = |url: &str| json!({"url": url, "issuer": "i", "clientId": "c", "tokenEndpoint": "t", "accessToken": "a", "obtainedAtMs": 1});
    std::fs::write(&creds, json!({"mcp": {"mobbin": entry("https://other.example/mcp"), "keep": entry("https://k.example/mcp")}, "other": 1}).to_string()).unwrap();
    assert_eq!(s.list().iter().find(|r| r["name"] == "mobbin").unwrap()["sign_in"], "signed in");
    let out = s.krowk(&["mcp", "remove", "mobbin"]);
    assert!(out.status.success(), "{}", text(&out));
    let after: Value = serde_json::from_slice(&std::fs::read(&creds).unwrap()).unwrap();
    assert_eq!(after, json!({"mcp": {"keep": entry("https://k.example/mcp")}, "other": 1}));
    assert!(s.list().iter().all(|r| r["name"] != "mobbin"));
}

#[test]
fn other_sources_are_listed_and_not_removed() {
    let s = Sandbox::new("sources");
    std::fs::write(s.root.join("repo/.mcp.json"), json!({"mcpServers": {"proj": {"command": "x"}}}).to_string()).unwrap();
    std::fs::write(s.root.join("home/.claude.json"), json!({"mcpServers": {"mine": {"type": "http", "url": "https://c.example/mcp"}}}).to_string()).unwrap();
    let rows = s.list();
    let proj = rows.iter().find(|r| r["name"] == "proj").unwrap();
    assert_eq!((proj["untrusted"].as_bool(), proj["source"].as_str()), (Some(true), Some(".mcp.json")));
    assert_eq!(rows.iter().find(|r| r["name"] == "mine").unwrap()["source"], "~/.claude.json");
    for name in ["proj", "mine"] {
        let out = s.krowk(&["mcp", "remove", name]);
        assert!(!out.status.success() && text(&out).contains("does not edit"), "{}", text(&out));
    }
    // Nothing was written, and a server nobody has is not found.
    assert!(!s.root.join("home/.krowk/config.json").exists());
    assert!(text(&s.krowk(&["mcp", "remove", "ghost"])).contains("no MCP server named ghost"));
    assert!(text(&s.krowk(&["help", "--all"])).contains("mcp add"));
}
