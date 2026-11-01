//! MCP, the whole path a person takes with Mobbin: the built `krowk` adds
//! the server and signs in by pasting the address the browser lands on; a
//! native turn calls `search_screens` through `mcp_call` and the Anthropic
//! request carries the image; a second turn reuses the connection; an
//! expired token refreshes without a word. Against Mobbin-shaped fakes.

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mobbin.rs"]
mod mobbin;
#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::permissions::Config;
use krowk_harness::protocol::{Command as Cmd, ModelRef, PermissionMode, RunResult};
use mock::Reply;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

fn krowk(root: &std::path::Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", root.join("home")).env("KROWK_NO_UPDATE_CHECK", "1").current_dir(root.join("repo"));
    c
}

/// The model: calls `search_screens` through `mcp_call` on a prompt, then answers.
fn model(body: &Value, _: usize) -> Reply {
    match body["messages"].as_array().and_then(|m| m.last()) {
        Some(last) if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) => Reply::sse(&mock::text_stream("Done.")),
        _ => Reply::sse(&mock::tool_use("toolu_s", "mcp_call", &json!({"tool": "mobbin:search_screens", "input": {"query": "onboarding"}}))),
    }
}

fn turn(rt: &tokio::runtime::Runtime, host: &Host, session: Option<&str>) -> RunResult {
    let cmd = Cmd::Prompt {
        session_id: session.map(String::from),
        text: "find onboarding screens on Mobbin".into(),
        images: Vec::new(),
        model: Some(ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() }),
        permission_mode: PermissionMode::BypassPermissions,
        toolset: None,
        effort: None,
        budget: None,
    };
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let r = host.execute(cmd, tx).await.unwrap().unwrap();
        while rx.try_recv().is_ok() {}
        r
    })
}

fn initializes(f: &mobbin::Fake) -> usize {
    f.mcp.seen.lock().unwrap().iter().filter(|s| s.body["method"] == "initialize" && s.header("authorization").is_some()).count()
}

fn calls(f: &mobbin::Fake) -> usize {
    f.mcp.seen.lock().unwrap().iter().filter(|s| s.body["method"] == "tools/call").count()
}

/// The image blocks in the tool result of the latest request to the model.
fn images(m: &mock::Mock) -> Vec<Value> {
    let seen = m.seen.lock().unwrap();
    let last = &seen.last().unwrap().body;
    last["messages"].as_array().unwrap().iter().flat_map(|msg| msg["content"].as_array().cloned().unwrap_or_default()).filter(|b| b["type"] == "tool_result").flat_map(|b| b["content"].as_array().cloned().unwrap_or_default()).filter(|p| p["type"] == "image").collect()
}

#[test]
fn mobbin_add_login_search_with_its_screens_a_reused_connection_and_a_silent_refresh() {
    let f = mobbin::fake(mobbin::Knobs { registration: true, ..Default::default() });
    let api = mock::serve(model);
    let root = std::env::temp_dir().join(format!("krowk-mcp-mobbin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    let root = root.canonicalize().unwrap();
    let url = format!("{}/mcp", f.mcp.url);

    // krowk mcp add, then krowk mcp login --no-browser with the address pasted.
    let out = krowk(&root, &["mcp", "add", "mobbin", &url, "--header", "X-Client: krowk-test"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let config_json = |root: &std::path::Path| -> Value { serde_json::from_slice(&std::fs::read(root.join("home/.krowk/config.json")).unwrap()).unwrap() };
    assert_eq!(config_json(&root)["mcpServers"]["mobbin"]["headers"]["X-Client"], "krowk-test");
    let mut login = krowk(&root, &["mcp", "login", "mobbin", "--no-browser"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stderr = std::io::BufReader::new(login.stderr.take().unwrap());
    let page = loop {
        let mut line = String::new();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "krowk printed no sign-in page");
        if line.starts_with("http") {
            break line.trim().to_string();
        }
    };
    let landed = mobbin::land(&f.auth.url, &page);
    writeln!(login.stdin.take().unwrap(), "{landed}").unwrap();
    let out = login.wait_with_output().unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut rest).unwrap();
    assert!(out.status.success(), "{}{rest}", String::from_utf8_lossy(&out.stdout));
    let creds = root.join("home/.krowk/credentials.json");
    let stored = |key: &str| -> Value { serde_json::from_str::<Value>(&std::fs::read_to_string(&creds).unwrap()).unwrap()["mcp"]["mobbin"][key].clone() };
    assert_eq!(stored("accessToken"), "at-secret-1");

    // A native turn, in the host a session runs in, on what the binary wrote.
    let config = config_json(&root);
    let env = {
        let (home, base) = (root.join("home").display().to_string(), api.url.clone());
        move |k: &str| match k {
            "HOME" => home.clone(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => base.clone(),
            _ => String::new(),
        }
    };
    let host = Host::new(HostConfig {
        sessions_dir: krowk_harness::log::sessions_dir(&env).unwrap(),
        cwd: root.join("repo"),
        registry: Registry::resolve(&InstancesConfig::default(), &env),
        krowk_version: "test".into(),
        pricer: Arc::new(|_, _, _| None),
        catalog: Arc::new(|_, _| None),
        credentials: creds.clone(),
        trust: krowk_harness::trust::allow_all(),
        publisher: None,
        permissions: Config { home: Some(root.join("home")), krowk_dir: Some(root.join("home/.krowk")), user: Some(config), trusted: Some(Arc::new(|_: &std::path::Path| true)), ..Config::default() },
        agents: krowk_harness::subagent::AgentsConfig::none(),
        session: Default::default(),
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let first = turn(&rt, &host, None);
    assert_eq!(first.result, "Done.");
    let shown = images(&api);
    assert_eq!(shown.len(), 1, "the screen reaches the model: {shown:?}");
    assert_eq!(shown[0]["source"]["media_type"], "image/png");
    assert_eq!(initializes(&f), 1);

    // The second turn reuses the connection.
    assert_eq!(turn(&rt, &host, Some(&first.session_id)).result, "Done.");
    assert_eq!(initializes(&f), 1, "one initialize across two turns");
    assert_eq!(calls(&f), 2);
    assert_eq!(images(&api).len(), 1);

    // An expired token refreshes on its own, with the rotated pair stored.
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&creds).unwrap()).unwrap();
    v["mcp"]["mobbin"]["expiresAtMs"] = json!(1);
    std::fs::write(&creds, v.to_string()).unwrap();
    let third = turn(&rt, &host, Some(&first.session_id));
    assert_eq!(third.result, "Done.");
    assert_eq!(calls(&f), 3);
    assert_eq!(images(&api).len(), 1);
    assert_eq!(stored("accessToken"), "at-secret-2");
    let refreshes: Vec<_> = f.auth.seen.lock().unwrap().iter().filter(|s| s.raw.contains("grant_type=refresh_token")).map(|s| s.raw.clone()).collect();
    assert_eq!(refreshes.len(), 1, "{refreshes:?}");
    assert!(refreshes[0].contains("refresh_token=rt-secret-1") && refreshes[0].contains("client_id=mobbin-client") && refreshes[0].contains("resource="), "{}", refreshes[0]);
    rt.block_on(host.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}
