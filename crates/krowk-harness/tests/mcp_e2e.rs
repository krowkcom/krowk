//! MCP servers through the host and the native loop (R-TOOL-3), against a
//! stand-in Anthropic API and a stand-in stdio MCP server
//! (`fixtures/mcp/fake-mcp`, a bash script): a repository's `.mcp.json`,
//! what trust turns on, the two deferred meta-tools the model is offered,
//! the permission rules its calls are judged by, and the servers a
//! subagent's definition lets it reach.
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
        self.mcp_json_of(&["fake"], tools);
    }

    /// A `.mcp.json` naming the stand-in server once by each of `names`,
    /// each logging to its own file (`log_of`).
    fn mcp_json_of(&self, names: &[&str], tools: usize) {
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/fake-mcp");
        let servers: serde_json::Map<String, Value> = names.iter().map(|n| (n.to_string(), json!({"command": "bash", "args": [fake], "env": {"FAKE_TOOLS": tools.to_string(), "FAKE_LOG": self.log_of(n), "FAKE_PID": self.root.join(format!("{n}.pid"))}}))).collect();
        std::fs::write(self.repo().join(".mcp.json"), json!({"mcpServers": servers}).to_string()).unwrap();
    }

    /// Where the stand-in server named `name` logs what it reads.
    fn log_of(&self, name: &str) -> PathBuf {
        match name {
            "fake" => self.fake_log(),
            n => self.root.join(format!("{n}-mcp.log")),
        }
    }

    fn host(&self, trusted: bool, user: Value) -> Host {
        self.host_by(Arc::new(move |_: &std::path::Path| trusted), user)
    }

    fn host_by(&self, trusted: Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>, user: Value) -> Host {
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
                trusted: Some(trusted),
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
    results_in(&seen.last().unwrap().body)
}

/// Every tool result one request carries, in order.
fn results_in(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
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

/// Whether a request is a subagent's: its system prompt says so.
fn is_child(body: &Value) -> bool {
    body["system"].to_string().contains("You are a subagent")
}

/// R-SUB-1 (ticket 25): a subagent whose definition names one MCP server
/// is offered the MCP tools for it alone — its search finds nothing on the
/// other, its call there is refused before it runs, and the other is never
/// started for it — while the session that started it still reaches both.
#[test]
fn r_sub_1_a_subagent_limited_to_one_mcp_server_cannot_call_another() {
    let parent = script(vec![
        ("subagent", json!({"description": "echo", "prompt": "echo hi through fake", "agent": "fake-only"})),
        ("mcp_call", json!({"tool": "other:echo", "input": {"text": "from the parent"}})),
    ]);
    let child = script(vec![
        ("mcp_search", json!({"query": "echo"})),
        ("mcp_call", json!({"tool": "other:echo", "input": {"text": "sneaked"}})),
        ("mcp_call", json!({"tool": "fake:echo", "input": {"text": "hi"}})),
    ]);
    // Whether the other server had been started when the parent's model is
    // handed the subagent's answer: read then, before its own call.
    let other_log: Arc<std::sync::Mutex<Option<PathBuf>>> = Arc::default();
    let started_before: Arc<std::sync::Mutex<Option<bool>>> = Arc::default();
    let (log, before) = (other_log.clone(), started_before.clone());
    let m = mock::serve(move |body, n| {
        if is_child(body) {
            return child(body, n);
        }
        if results_in(body).len() == 1 {
            *before.lock().unwrap() = log.lock().unwrap().as_ref().map(|p| p.exists());
        }
        parent(body, n)
    });
    let h = Home::new("subagent-allowlist", &m.url);
    h.mcp_json_of(&["fake", "other"], 1);
    *other_log.lock().unwrap() = Some(h.log_of("other"));
    std::fs::create_dir_all(h.repo().join(".krowk/agents")).unwrap();
    std::fs::write(h.repo().join(".krowk/agents/fake-only.md"), "---\nname: fake-only\ndescription: Echoes through fake.\ntools: Read, mcp__fake\n---\nEcho.\n").unwrap();
    let host = h.host(true, json!({}));
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let seen: Vec<Value> = m.seen.lock().unwrap().iter().map(|s| s.body.clone()).collect();
    let children: Vec<&Value> = seen.iter().filter(|b| is_child(b)).collect();
    let tools: Vec<&Value> = children[0]["tools"].as_array().unwrap().iter().collect();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read", "mcp_search", "mcp_call"], "its list, and the MCP tools for the server it names");
    let search = tools.iter().find(|t| t["name"] == "mcp_search").unwrap()["description"].as_str().unwrap();
    assert!(search.contains("(fake)") && !search.contains("other"), "only its server is named to it: {search}");
    let sent = results_in(children.last().unwrap());
    assert!(sent[0].contains("\"fake:echo\"") && !sent[0].contains("other"), "its search finds nothing on the other server: {}", sent[0]);
    assert_eq!(sent[1], "other:echo was not run: agent fake-only reaches only the MCP tools its definition names — fake (any tool)", "refused before it ran, even in bypass mode");
    assert_eq!(sent[2], "echo: hi", "its own server answers");
    assert_eq!(*started_before.lock().unwrap(), Some(true), "the session's own turn connects every server in the background (R-MCP-3), the subagent's allowlist or not");
    let other = std::fs::read_to_string(h.log_of("other")).unwrap_or_default();
    assert!(!other.contains("sneaked"), "the refused call never reached the other server: {other}");
    // The session that started it reaches every server still, the other
    // started for it now.
    let parent_sent = results_in(seen.iter().rfind(|b| !is_child(b)).unwrap());
    assert_eq!(parent_sent[1], "echo: from the parent", "{parent_sent:?}");
    assert!(std::fs::read_to_string(h.log_of("other")).unwrap().contains("tools/call"));
    // The subagent used the session's connection to `fake`, not one of its
    // own.
    assert_eq!(count(&h.fake_log(), "initialize"), 1, "one connection to fake, shared by the parent and its subagent");
}

/// R-SUB-1 (ticket 25): a subagent whose definition names one MCP tool,
/// `mcp__fake__echo`, finds that tool alone on its server and is refused
/// its sibling, before the server sees the call.
#[test]
fn r_sub_1_a_subagent_limited_to_one_mcp_tool_cannot_call_its_sibling() {
    let parent = script(vec![("subagent", json!({"description": "echo", "prompt": "echo hi", "agent": "echo-only"}))]);
    let child = script(vec![
        ("mcp_search", json!({"query": "filler echo"})),
        ("mcp_call", json!({"tool": "fake:tool_2", "input": {"q": "sneaked"}})),
        ("mcp_call", json!({"tool": "fake:echo", "input": {"text": "hi"}})),
    ]);
    let m = mock::serve(move |body, n| if is_child(body) { child(body, n) } else { parent(body, n) });
    let h = Home::new("subagent-one-tool", &m.url);
    h.mcp_json(2);
    std::fs::create_dir_all(h.repo().join(".claude/agents")).unwrap();
    std::fs::write(h.repo().join(".claude/agents/echo-only.md"), "---\nname: echo-only\ndescription: Echoes.\ntools: mcp__fake__echo\n---\nEcho.\n").unwrap();
    let host = h.host(true, json!({}));
    let r = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    let seen: Vec<Value> = m.seen.lock().unwrap().iter().map(|s| s.body.clone()).collect();
    let sent = results_in(seen.iter().rfind(|b| is_child(b)).unwrap());
    assert!(sent[0].contains("\"fake:echo\"") && !sent[0].contains("tool_2"), "its search finds the one tool, not its sibling: {}", sent[0]);
    assert_eq!(sent[1], "fake:tool_2 was not run: agent echo-only reaches only the MCP tools its definition names — fake:echo");
    assert_eq!(sent[2], "echo: hi");
    let log = std::fs::read_to_string(h.fake_log()).unwrap();
    assert!(!log.contains("sneaked") && log.matches("tools/call").count() == 1, "only the allowed call reached the server: {log}");
}

/// A model that answers each prompt with one `mcp_search`, which starts
/// every server, then "Done.": a turn that touches all the servers.
fn search_each_turn(body: &Value, _: usize) -> Reply {
    match body["messages"].as_array().and_then(|m| m.last()) {
        Some(last) if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) => Reply::sse(&mock::text_stream("Done.")),
        _ => Reply::sse(&mock::tool_use("toolu_s", "mcp_search", &json!({"query": "echo"}))),
    }
}

/// One turn of a session, on a runtime that outlives it: the host is not
/// shut down, so the session's MCP connections stay.
fn turn(rt: &tokio::runtime::Runtime, host: &Host, session: Option<&str>) -> RunResult {
    let Command::Prompt { text, model, permission_mode, .. } = prompt(PermissionMode::BypassPermissions) else { unreachable!() };
    let cmd = Command::Prompt { session_id: session.map(String::from), text, images: Vec::new(), model, permission_mode, toolset: None, effort: None, budget: None };
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let r = host.execute(cmd, tx).await.unwrap().unwrap();
        while rx.try_recv().is_ok() {}
        r
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn count(log: &std::path::Path, method: &str) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().matches(&format!("\"method\":\"{method}\"")).count()
}

/// The process ids a stand-in server logged, oldest first.
fn pids(h: &Home, name: &str) -> Vec<i32> {
    std::fs::read_to_string(h.root.join(format!("{name}.pid"))).unwrap_or_default().lines().map(|l| l.parse().unwrap()).collect()
}

fn alive(pid: i32) -> bool {
    // A zombie has been killed; only its parent has yet to reap it.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(st) => st.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
        Err(_) => false,
    }
}

fn gone(pid: i32) -> bool {
    (0..60).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        !alive(pid)
    })
}

/// A streamable-HTTP stand-in that gives out session id `s-1`.
fn http_server() -> mock::Mock {
    mock::serve_seen(|seen, _| {
        let id = seen.body["id"].clone();
        match seen.body["method"].as_str() {
            Some("initialize") => Reply { headers: vec![("mcp-session-id".into(), "s-1".into())], ..Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18", "capabilities": {}}})) },
            Some("tools/list") => Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "echo", "description": "Echo a text back.", "inputSchema": {"type": "object"}}]}})),
            _ => Reply::status(202, ""),
        }
    })
}

fn http_count(server: &mock::Mock, method: &str) -> usize {
    server.seen.lock().unwrap().iter().filter(|s| s.method == method || s.body["method"] == method).count()
}

/// D5, R-MCP-1: two turns of one session start each server once — one
/// process and one `initialize` for the stdio server, one `initialize` for
/// the HTTP one.
#[test]
fn r_mcp_1_two_turns_of_a_session_start_each_server_once() {
    let web = http_server();
    let m = mock::serve(search_each_turn);
    let h = Home::new("reuse", &m.url);
    h.mcp_json(1);
    let host = h.host(true, json!({"mcpServers": {"web": {"type": "http", "url": web.url}}}));
    let rt = runtime();
    let first = turn(&rt, &host, None);
    let second = turn(&rt, &host, Some(&first.session_id));
    assert_eq!(second.result, "Done.");
    assert_eq!(count(&h.fake_log(), "initialize"), 1, "the stdio server was initialized once");
    assert_eq!(pids(&h, "fake").len(), 1, "and spawned once");
    assert_eq!(http_count(&web, "initialize"), 1, "the HTTP server was initialized once");
    assert_eq!(count(&h.fake_log(), "tools/list"), 1, "its tools are not listed again either");
    rt.block_on(host.shutdown());
}

/// D5, R-MCP-2: between turns, a changed server restarts alone; a removed
/// one and one of a repository that is no longer trusted are closed before
/// the next model call.
#[test]
fn r_mcp_2_a_changed_server_restarts_alone_and_a_removed_or_untrusted_one_is_closed() {
    let m = mock::serve(search_each_turn);
    let h = Home::new("changes", &m.url);
    h.mcp_json_of(&["a", "b", "c"], 1);
    let trusted = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let t = trusted.clone();
    let host = h.host_by(Arc::new(move |_: &std::path::Path| t.load(std::sync::atomic::Ordering::SeqCst)), json!({}));
    let rt = runtime();
    let id = turn(&rt, &host, None).session_id;
    // b's configuration changes: only b starts again.
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(h.repo().join(".mcp.json")).unwrap()).unwrap();
    v["mcpServers"]["b"]["env"]["FAKE_TOOLS"] = json!("2");
    std::fs::write(h.repo().join(".mcp.json"), v.to_string()).unwrap();
    turn(&rt, &host, Some(&id));
    assert_eq!((count(&h.log_of("a"), "initialize"), count(&h.log_of("b"), "initialize"), count(&h.log_of("c"), "initialize")), (1, 2, 1));
    assert!(gone(pids(&h, "b")[0]), "b's old process was stopped");
    // c is removed: its process is gone before the turn's model call.
    v["mcpServers"].as_object_mut().unwrap().remove("c");
    std::fs::write(h.repo().join(".mcp.json"), v.to_string()).unwrap();
    turn(&rt, &host, Some(&id));
    assert!(gone(pids(&h, "c")[0]), "c was closed");
    assert!(alive(pids(&h, "a")[0]), "a was not touched");
    // The repository is no longer trusted: its servers are closed, and the
    // model is offered no MCP tool.
    trusted.store(false, std::sync::atomic::Ordering::SeqCst);
    turn(&rt, &host, Some(&id));
    assert!(gone(pids(&h, "a")[0]), "a was closed once its repository was untrusted");
    let seen = m.seen.lock().unwrap();
    let last = seen.last().unwrap().body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(!last.iter().any(|t| t.starts_with("mcp_")), "{last:?}");
    drop(seen);
    rt.block_on(host.shutdown());
}

/// R-MCP-5: the idle sweep closes a session's servers, and so does the
/// host's shutdown — processes stopped, an HTTP session ended with DELETE.
#[test]
fn r_mcp_5_the_idle_sweep_and_shutdown_close_a_sessions_servers() {
    let web = http_server();
    let m = mock::serve(search_each_turn);
    let h = Home::new("close", &m.url);
    h.mcp_json(1);
    let host = h.host(true, json!({"mcpServers": {"web": {"type": "http", "url": web.url}}})).with_backend_idle(std::time::Duration::ZERO);
    let rt = runtime();
    turn(&rt, &host, None);
    assert_eq!(http_count(&web, "DELETE"), 0, "a session's connections stay between its turns");
    // Another session's prompt sweeps the idle one.
    turn(&rt, &host, None);
    let first = pids(&h, "fake")[0];
    assert!(gone(first), "the idle session's process was stopped");
    assert_eq!(http_count(&web, "DELETE"), 1, "its HTTP session was ended");
    assert!(web.seen.lock().unwrap().iter().any(|s| s.method == "DELETE" && s.header("mcp-session-id") == Some("s-1")));
    let second = pids(&h, "fake")[1];
    assert!(alive(second));
    rt.block_on(host.shutdown());
    assert!(gone(second), "shutdown stopped the other's process");
    assert_eq!(http_count(&web, "DELETE"), 2);
}
