//! MCP servers through the host and the native loop (R-TOOL-3), against a
//! stand-in Anthropic API and a stand-in stdio MCP server
//! (`fixtures/mcp/fake-mcp`, a bash script): a repository's `.mcp.json`,
//! what trust turns on, the two deferred meta-tools the model is offered,
//! and the permission rules its calls are judged by.
#![cfg(unix)]

#[path = "common/mock.rs"]
mod mock;

use krowk_harness::host::{Host, HostConfig};
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::permissions::Config;
use krowk_harness::protocol::{Command, ContextRecord, ModelRef, PermissionMode, RunResult};
use mock::Reply;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

struct Home {
    root: PathBuf,
    url: String,
}

impl Home {
    fn new(name: &str, url: &str) -> Home {
        let root =
            std::env::temp_dir().join(format!("krowk-mcp-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        Home {
            root: root.canonicalize().unwrap(),
            url: url.into(),
        }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "ANTHROPIC_API_KEY" => "sk-test".into(),
            "ANTHROPIC_BASE_URL" => self.url.clone(),
            _ => String::new(),
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn fake_log(&self) -> PathBuf {
        self.root.join("fake-mcp.log")
    }

    /// A `.mcp.json` naming the stand-in server as `fake`, listing `tools`.
    fn mcp_json(&self, tools: usize) {
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/fake-mcp");
        let body = json!({"mcpServers": {"fake": {"command": "bash", "args": [fake], "env": {"FAKE_TOOLS": tools.to_string(), "FAKE_LOG": self.fake_log()}}}});
        std::fs::write(self.repo().join(".mcp.json"), body.to_string()).unwrap();
    }

    fn host(&self, trusted: bool, user: Value) -> Host {
        Host::new(HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.repo(),
            registry: Registry::resolve(&InstancesConfig::default(), &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.krowk/credentials.json"),
            trust: krowk_harness::trust::allow_all(),
            publisher: None,
            permissions: Config {
                home: Some(self.root.join("home")),
                krowk_dir: Some(self.root.join("home/.krowk")),
                user: Some(user),
                trusted: Some(Arc::new(move |_: &std::path::Path| trusted)),
                ..Config::default()
            },
            agents: krowk_harness::subagent::AgentsConfig::none(),
            session: Default::default(),
        })
    }

    fn context(&self, session: &str) -> Vec<ContextRecord> {
        let dir = log::sessions_dir(&self.env()).unwrap().join(session);
        std::fs::read_to_string(dir.join(log::CONTEXT_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prompt(mode: PermissionMode) -> Command {
    Command::Prompt {
        session_id: None,
        text: "echo hi through MCP".into(), images: Vec::new(),
        model: Some(ModelRef {
            instance: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
        }),
        permission_mode: mode,
        toolset: None,
        effort: None,
        budget: None,
    }
}

/// A model that calls each tool in turn, one per request, then answers.
fn script(calls: Vec<(&'static str, Value)>) -> impl Fn(&Value, usize) -> Reply + Send + 'static {
    move |body, _| {
        let done = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"))
            })
            .count();
        match calls.get(done) {
            Some((name, input)) => {
                Reply::sse(&mock::tool_use(&format!("toolu_{done}"), name, input))
            }
            None => Reply::sse(&mock::text_stream("Done.")),
        }
    }
}

fn run(host: &Host, cmd: Command) -> RunResult {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let r = host.execute(cmd, tx).await.unwrap().unwrap();
        while rx.try_recv().is_ok() {}
        host.shutdown().await;
        r
    })
}

/// Every tool result the model was sent, in order.
fn results(m: &mock::Mock) -> Vec<String> {
    let seen = m.seen.lock().unwrap();
    let last = seen.last().unwrap().body["messages"]
        .as_array()
        .unwrap()
        .clone();
    last.iter()
        .flat_map(|msg| msg["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| match &b["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            v => v.to_string(),
        })
        .collect()
}

/// The names of the tools the model was offered on its first request.
fn offered(m: &mock::Mock) -> Vec<String> {
    let seen = m.seen.lock().unwrap();
    seen[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// `context.tokens` in krowk-bench's budgets.toml: ticket 03's ceiling on
/// the system prompt plus the tool definitions.
fn context_tokens_budget() -> u64 {
    let file = include_str!("../../krowk-bench/budgets.toml");
    let at = file.find("id = \"context.tokens\"").unwrap();
    file[at..]
        .lines()
        .find_map(|l| l.strip_prefix("max = "))
        .unwrap()
        .trim()
        .replace('_', "")
        .parse()
        .unwrap()
}

#[test]
fn r_tool_3_a_trusted_projects_mcp_server_is_found_by_search_and_called() {
    let m = mock::serve(script(vec![
        ("mcp_search", json!({"query": "echo text"})),
        (
            "mcp_call",
            json!({"tool": "fake:echo", "input": {"text": "hi"}}),
        ),
    ]));
    let h = Home::new("found", &m.url);
    h.mcp_json(1);
    // The person allows this one MCP tool; default mode would ask otherwise.
    let host = h.host(true, json!({"permissions": {"allow": ["Mcp(fake:echo)"]}}));
    let r = run(&host, prompt(PermissionMode::Default));
    assert_eq!(r.result, "Done.");
    let tools = offered(&m);
    assert!(
        tools.contains(&"mcp_search".into()) && tools.contains(&"mcp_call".into()),
        "{tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t.contains("echo")),
        "the server's tool is deferred, not offered: {tools:?}"
    );
    let sent = results(&m);
    assert!(
        sent[0].contains("\"fake:echo\"") && sent[0].contains("\"input_schema\""),
        "search returns the match with its schema: {}",
        sent[0]
    );
    assert_eq!(
        sent[1], "echo: hi",
        "the call reached the server and its answer came back"
    );
    let log = std::fs::read_to_string(h.fake_log()).unwrap();
    assert!(
        log.contains("\"method\":\"initialize\"") && log.contains("\"method\":\"tools/call\""),
        "{log}"
    );
}

#[test]
fn r_tool_3_an_untrusted_repositorys_mcp_json_starts_nothing() {
    let m = mock::serve(script(vec![("mcp_search", json!({"query": "echo"}))]));
    let h = Home::new("untrusted", &m.url);
    h.mcp_json(1);
    let host = h.host(false, json!({}));
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    assert!(
        !offered(&m).iter().any(|t| t.starts_with("mcp_")),
        "no MCP tools before trust: {:?}",
        offered(&m)
    );
    assert!(!h.fake_log().exists(), "the server was never started");
    // The trust question is asked for it.
    let probe = Config {
        home: Some(h.root.join("home")),
        ..Config::default()
    };
    assert!(
        krowk_harness::permissions::settings::widens(&probe, &h.repo()),
        "a .mcp.json is a reason to ask about trust"
    );
}

#[test]
fn r_tool_3_fifty_mcp_tools_cost_the_context_two_definitions_within_the_ticket_3_budget() {
    let m = mock::serve(script(vec![("mcp_search", json!({"query": "filler"}))]));
    let h = Home::new("fifty", &m.url);
    h.mcp_json(50);
    let host = h.host(true, json!({}));
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let sent = results(&m);
    assert!(
        sent[0].contains("fake:tool_"),
        "the fifty were listed and searched: {}",
        sent[0]
    );
    assert_eq!(
        sent[0].matches("\"tool\":").count(),
        5,
        "search returns the best five, not all fifty"
    );
    let ctx = h.context(&r.session_id);
    let budget = context_tokens_budget();
    for c in &ctx {
        let names: Vec<&str> = c.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"mcp_search") && !names.iter().any(|n| n.starts_with("tool_")),
            "{names:?}"
        );
        let total = c.system_tokens + c.tools_tokens;
        println!(
            "R-TOOL-3 context tokens with 50 MCP tools: system {} + tools {} = {total} (budget {budget})",
            c.system_tokens, c.tools_tokens
        );
        assert!(
            total <= budget,
            "{total} tokens with 50 MCP tools configured, over context.tokens' {budget}"
        );
    }
}

#[test]
fn r_tool_3_plan_mode_and_a_whole_server_deny_start_no_server() {
    for (mode, user) in [(PermissionMode::Plan, json!({})), (PermissionMode::BypassPermissions, json!({"permissions": {"deny": ["Mcp(fake)"]}}))] {
        let m = mock::serve(script(vec![("mcp_search", json!({"query": "echo"}))]));
        let h = Home::new("nostart", &m.url);
        h.mcp_json(1);
        let host = h.host(true, user.clone());
        let r = run(&host, prompt(mode));
        assert_eq!(r.result, "Done.");
        assert!(!h.fake_log().exists(), "{mode:?} {user}: the server was started");
        assert!(!results(&m)[0].contains("fake:echo"), "{mode:?}: {}", results(&m)[0]);
    }
}

#[test]
fn r_tool_3_a_deny_rule_on_mcp_server_tool_blocks_the_call() {
    let m = mock::serve(script(vec![
        ("mcp_search", json!({"query": "echo"})),
        (
            "mcp_call",
            json!({"tool": "fake:echo", "input": {"text": "hi"}}),
        ),
    ]));
    let h = Home::new("deny", &m.url);
    h.mcp_json(1);
    let host = h.host(true, json!({"permissions": {"deny": ["Mcp(fake:echo)"]}}));
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let sent = results(&m);
    assert!(
        !sent[0].contains("\"fake:echo\""),
        "search does not offer a denied tool: {}",
        sent[0]
    );
    assert!(
        sent[1].contains("denied by the rule `Mcp(fake:echo)`"),
        "{}",
        sent[1]
    );
    let log = std::fs::read_to_string(h.fake_log()).unwrap_or_default();
    assert!(
        !log.contains("tools/call"),
        "the call never reached the server: {log}"
    );
}

#[test]
fn r_tool_3_a_streamable_http_server_from_the_persons_own_config_is_called() {
    // The server answers JSON, and `tools/call` as an SSE stream; its
    // session id is carried from `initialize` on.
    let server = mock::serve_seen(|seen, _| {
        let id = seen.body["id"].clone();
        match seen.body["method"].as_str() {
            Some("initialize") => Reply {
                headers: vec![("mcp-session-id".into(), "s-1".into())],
                ..Reply::json(
                    200,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18", "capabilities": {}}}),
                )
            },
            Some("tools/list") => Reply::json(
                200,
                &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "echo", "description": "Echo a text back.", "inputSchema": {"type": "object"}}]}}),
            ),
            Some("tools/call") if seen.header("mcp-session-id") == Some("s-1") => {
                Reply::sse(&format!(
                    "event: message\ndata: {}\n\n",
                    json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("echo: {}", seen.body["params"]["arguments"]["text"].as_str().unwrap_or_default())}]}})
                ))
            }
            Some("tools/call") => Reply::status(400, "no session"),
            _ => Reply::status(202, ""),
        }
    });
    let m = mock::serve(script(vec![(
        "mcp_call",
        json!({"tool": "web:echo", "input": {"text": "over http"}}),
    )]));
    let h = Home::new("http", &m.url);
    // krowk's own config.json, which needs no trust: it is the person's.
    let host = h.host(
        false,
        json!({"mcpServers": {"web": {"type": "http", "url": server.url}}}),
    );
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    assert_eq!(results(&m)[0], "echo: over http");
}
