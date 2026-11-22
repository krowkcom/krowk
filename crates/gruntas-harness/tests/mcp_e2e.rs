//! MCP servers through the host (R-TOOL-3, R-MCP), for a session running
//! in Claude Code — the fake `claude` (`common/fake.rs`) — against a
//! stand-in stdio MCP server (`fixtures/mcp/fake-mcp`, a bash script) and a
//! stand-in streamable-HTTP one: a repository's `.mcp.json`, what trust
//! turns on, the two deferred meta-tools krowk's bridge offers Claude Code,
//! the permission rules a call is judged by, and the life of a session's
//! connections across its turns.
//!
//! Claude Code is started `--strict-mcp-config` with only krowk's bridge,
//! so every server here is krowk's to start, judge and close: the model's
//! `mcp_search` and `mcp_call` are Claude Code's calls of
//! `mcp__krowk__mcp_search` and `mcp__krowk__mcp_call`, asked about with
//! `can_use_tool` and run over the bridge (`mcp_message`).
#![cfg(unix)]

#[path = "common/fake.rs"]
mod fake;
#[path = "common/mock.rs"]
mod mock;

use gruntas_harness::host::{Host, HostConfig};
use gruntas_harness::instances::{InstancesConfig, Registry};
use gruntas_harness::log;
use gruntas_harness::permissions::Config;
use gruntas_harness::protocol::{Command, Item, LogBody, LogEvent, ModelRef, PermissionMode, RunResult, StreamLine};
use mock::Reply;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

struct Home {
    root: PathBuf,
    instances: InstancesConfig,
}

impl Home {
    /// A home whose one instance, `claude`, is the fake playing `scenario`.
    fn new(name: &str, scenario: Option<&str>) -> Home {
        let root =
            std::env::temp_dir().join(format!("krowk-mcp-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        let root = root.canonicalize().unwrap();
        let instances = InstancesConfig { instances: [("claude".to_string(), fake::claude(&root, "claude", scenario))].into_iter().collect(), ..Default::default() };
        Home { root, instances }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => std::env::var("PATH").unwrap_or_default(),
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
            registry: Registry::resolve(&self.instances, &self.env()),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.root.join("home/.gruntas/credentials.json"),
            trust: gruntas_harness::trust::allow_all(),
            publisher: None,
            permissions: Config {
                home: Some(self.root.join("home")),
                krowk_dir: Some(self.root.join("home/.gruntas")),
                user: Some(user),
                trusted: Some(trusted),
                ..Config::default()
            },
            agents: gruntas_harness::agents::AgentsConfig::none(),
            session: Default::default(),
        })
    }

    /// The names of the tools krowk's bridge last listed to Claude Code
    /// (`tools/list`, asked when a process starts): krowk's own and, when
    /// the session has servers, the two MCP meta-tools.
    fn listed(&self) -> Vec<String> {
        let log = fake::log(&self.root, "claude");
        let tools = log.lines().filter_map(|l| l.strip_prefix("answer ")).filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| v.pointer("/response/response/mcp_response/result/tools").cloned()).next_back().expect("the bridge listed its tools");
        tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prompt(mode: PermissionMode) -> Command {
    Command::Prompt {
        rollover: None,
        session_id: None,
        text: "echo hi through MCP".into(), images: Vec::new(),
        model: Some(ModelRef {
            instance: "claude".into(),
            model: "sonnet".into(),
        }),
        permission_mode: mode,
        toolset: None,
        effort: None,
        budget: None,
    }
}

/// The model calls krowk's bridged `tool` (`mcp_search` or `mcp_call`)
/// with `input`: Claude Code asks krowk whether it may (`can_use_tool`),
/// runs it over the bridge when `runs`, and the model reads what came back
/// — the bridge's answer, or the denial's message.
fn bridged(id: &str, tool: &str, input: Value, runs: bool) -> String {
    let name = format!("mcp__krowk__{tool}");
    let call = json!({"type": "assistant", "message": {"id": format!("msg_{id}"), "model": "{{MODEL}}", "role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}, "parent_tool_use_id": null, "session_id": "{{SESSION}}"});
    let ask = json!({"subtype": "can_use_tool", "tool_name": name, "mcp_server": {"name": "krowk", "source": "sdk"}, "input": input, "tool_use_id": id});
    let run = json!({"subtype": "mcp_message", "server_name": "krowk", "message": {"method": "tools/call", "params": {"name": tool, "arguments": input, "_meta": {"claudecode/toolUseId": id}}, "jsonrpc": "2.0", "id": 2}});
    if runs { format!("{call}\n@ask {ask}\n@ask {run}\n@tool_result {id}") } else { format!("{call}\n@ask {ask}\n@tool_result {id}") }
}

/// A model that calls each tool in turn, then answers "Done.".
fn script(calls: Vec<(&str, Value)>) -> String {
    let mut s: Vec<String> = calls.into_iter().enumerate().map(|(i, (tool, input))| bridged(&format!("toolu_{i}"), tool, input, true)).collect();
    s.push(fake::answer("Done."));
    s.join("\n")
}

/// A model that answers each prompt with one `mcp_search`, which starts
/// every server, then "Done.": a turn that touches all the servers.
fn search_each_turn() -> String {
    format!("@when echo hi\n{}", script(vec![("mcp_search", json!({"query": "echo"}))]))
}

/// Runs a prompt to its end, then shuts the host down: its result, and
/// what the model read back from each tool, in order.
fn run(host: &Host, cmd: Command) -> (RunResult, Vec<String>) {
    let rt = runtime();
    rt.block_on(async {
        let (lines, r) = execute(host, cmd).await;
        host.shutdown().await;
        (r, results(&lines))
    })
}

async fn execute(host: &Host, cmd: Command) -> (Vec<StreamLine>, RunResult) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let collect = async {
        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        lines
    };
    let (r, lines) = tokio::join!(host.execute(cmd, tx), collect);
    (lines, r.unwrap().unwrap())
}

/// Every tool result of a turn, in order: what Claude Code handed the model.
fn results(lines: &[StreamLine]) -> Vec<String> {
    lines.iter().filter_map(|l| match l {
        StreamLine::Log(LogEvent { body: LogBody::ItemCompleted { item: Item::ToolResult { output, .. }, .. }, .. }) => Some(output.clone()),
        _ => None,
    }).collect()
}

#[test]
fn r_tool_3_a_trusted_projects_mcp_server_is_found_by_search_and_called() {
    let h = Home::new("found", Some(&script(vec![
        ("mcp_search", json!({"query": "echo text"})),
        (
            "mcp_call",
            json!({"tool": "fake:echo", "input": {"text": "hi"}}),
        ),
    ])));
    h.mcp_json(1);
    // The person allows this one MCP tool; default mode would ask otherwise.
    let host = h.host(true, json!({"permissions": {"allow": ["Mcp(fake:echo)"]}}));
    let (r, sent) = run(&host, prompt(PermissionMode::Default));
    assert_eq!(r.result, "Done.");
    let tools = h.listed();
    assert!(
        tools.contains(&"mcp_search".into()) && tools.contains(&"mcp_call".into()),
        "{tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t.contains("echo")),
        "the server's tool is deferred, not offered: {tools:?}"
    );
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
    // Claude Code loads no server itself: only krowk's bridge is passed.
    let argv = fake::log(&h.root, "claude").lines().find(|l| l.starts_with("argv -p ")).unwrap().to_string();
    assert!(argv.contains("--strict-mcp-config") && !argv.contains("fake-mcp"), "{argv}");
}

#[test]
fn r_tool_3_an_untrusted_repositorys_mcp_json_starts_nothing() {
    let h = Home::new("untrusted", Some(&fake::answer("Done.")));
    h.mcp_json(1);
    let host = h.host(false, json!({}));
    let (r, _) = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    assert!(
        !h.listed().iter().any(|t| t.starts_with("mcp_")),
        "no MCP tools before trust: {:?}",
        h.listed()
    );
    assert!(!h.fake_log().exists(), "the server was never started");
    // The trust question is asked for it.
    let probe = Config {
        home: Some(h.root.join("home")),
        ..Config::default()
    };
    assert!(
        gruntas_harness::permissions::settings::widens(&probe, &h.repo()),
        "a .mcp.json is a reason to ask about trust"
    );
}

#[test]
fn r_tool_3_plan_mode_and_a_whole_server_deny_start_no_server() {
    for (mode, user) in [(PermissionMode::Plan, json!({})), (PermissionMode::BypassPermissions, json!({"permissions": {"deny": ["Mcp(fake)"]}}))] {
        let h = Home::new("nostart", Some(&script(vec![("mcp_search", json!({"query": "echo"}))])));
        h.mcp_json(1);
        let host = h.host(true, user.clone());
        let (r, sent) = run(&host, prompt(mode));
        assert_eq!(r.result, "Done.");
        assert!(!h.fake_log().exists(), "{mode:?} {user}: the server was started");
        assert!(!sent[0].contains("fake:echo"), "{mode:?}: {}", sent[0]);
        if mode == PermissionMode::Plan {
            assert!(sent[0].contains("plan mode runs no commands"), "the bridge refuses the search that would start it: {}", sent[0]);
        } else {
            assert!(!h.listed().iter().any(|t| t.starts_with("mcp_")), "a server denied whole is not offered: {:?}", h.listed());
        }
    }
}

#[test]
fn r_tool_3_a_deny_rule_on_mcp_server_tool_blocks_the_call() {
    // Claude Code is told no at `can_use_tool`, and calls nothing.
    let scenario = [bridged("toolu_0", "mcp_search", json!({"query": "echo"}), true), bridged("toolu_1", "mcp_call", json!({"tool": "fake:echo", "input": {"text": "hi"}}), false), fake::answer("Done.")].join("\n");
    let h = Home::new("deny", Some(&scenario));
    h.mcp_json(1);
    let host = h.host(true, json!({"permissions": {"deny": ["Mcp(fake:echo)"]}}));
    let (r, sent) = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
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
    let h = Home::new("http", Some(&script(vec![(
        "mcp_call",
        json!({"tool": "web:echo", "input": {"text": "over http"}}),
    )])));
    // krowk's own config.json, which needs no trust: it is the person's.
    let host = h.host(
        false,
        json!({"mcpServers": {"web": {"type": "http", "url": server.url}}}),
    );
    let (r, sent) = run(&host, prompt(PermissionMode::BypassPermissions));
    assert_eq!(r.result, "Done.");
    assert_eq!(sent[0], "echo: over http");
}

/// One turn of a session, on a runtime that outlives it: the host is not
/// shut down, so the session's MCP connections stay.
fn turn(rt: &tokio::runtime::Runtime, host: &Host, session: Option<&str>) -> RunResult {
    let Command::Prompt { text, model, permission_mode, .. } = prompt(PermissionMode::BypassPermissions) else { unreachable!() };
    let cmd = Command::Prompt { session_id: session.map(String::from), text, images: Vec::new(), model, permission_mode, toolset: None, effort: None, rollover: None, budget: None };
    rt.block_on(execute(host, cmd)).1
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
    // macOS has no /proc, so there `ps` says the state.
    if cfg!(target_os = "linux") {
        return match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(st) => st.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
            Err(_) => false,
        };
    }
    match std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output() {
        Ok(o) => {
            let st = String::from_utf8_lossy(&o.stdout);
            let st = st.trim();
            !st.is_empty() && !st.starts_with('Z')
        }
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
    let h = Home::new("reuse", Some(&search_each_turn()));
    h.mcp_json(1);
    let host = h.host(true, json!({"mcpServers": {"web": {"type": "http", "url": web.url}}}));
    let rt = runtime();
    let first = turn(&rt, &host, None);
    let second = turn(&rt, &host, Some(&first.session_id));
    assert_eq!(second.result, "Done.");
    assert_eq!(fake::prompts(&h.root, "claude").len(), 2, "both turns reached Claude Code");
    assert_eq!(count(&h.fake_log(), "initialize"), 1, "the stdio server was initialized once");
    assert_eq!(pids(&h, "fake").len(), 1, "and spawned once");
    assert_eq!(http_count(&web, "initialize"), 1, "the HTTP server was initialized once");
    assert_eq!(count(&h.fake_log(), "tools/list"), 1, "its tools are not listed again either");
    rt.block_on(host.shutdown());
}

/// D5, R-MCP-2: between turns, a changed server restarts alone; a removed
/// one and one of a repository that is no longer trusted are closed when
/// the next turn starts, and Claude Code is offered no MCP tool for them.
#[test]
fn r_mcp_2_a_changed_server_restarts_alone_and_a_removed_or_untrusted_one_is_closed() {
    let h = Home::new("changes", Some(&search_each_turn()));
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
    // c is removed: its process is gone.
    v["mcpServers"].as_object_mut().unwrap().remove("c");
    std::fs::write(h.repo().join(".mcp.json"), v.to_string()).unwrap();
    turn(&rt, &host, Some(&id));
    assert!(gone(pids(&h, "c")[0]), "c was closed");
    assert!(alive(pids(&h, "a")[0]), "a was not touched");
    // The repository is no longer trusted: its servers are closed, and
    // Claude Code is offered no MCP tool.
    trusted.store(false, std::sync::atomic::Ordering::SeqCst);
    turn(&rt, &host, Some(&id));
    assert!(gone(pids(&h, "a")[0]), "a was closed once its repository was untrusted");
    let last = h.listed();
    assert!(!last.iter().any(|t| t.starts_with("mcp_")), "{last:?}");
    rt.block_on(host.shutdown());
}

/// R-MCP-5: the idle sweep closes a session's servers, and so does the
/// host's shutdown — processes stopped, an HTTP session ended with DELETE.
#[test]
fn r_mcp_5_the_idle_sweep_and_shutdown_close_a_sessions_servers() {
    let web = http_server();
    let h = Home::new("close", Some(&search_each_turn()));
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

/// R-MCP-21: the host reports a session's servers — name, where each is
/// configured, status and tools — and starts one again on request.
#[test]
fn r_mcp_21_the_host_reports_the_sessions_servers_and_reconnects_one() {
    use gruntas_harness::protocol::McpState;
    let h = Home::new("view", None);
    h.mcp_json(2);
    let host = h.host(true, json!({}));
    let rt = runtime();
    rt.block_on(async {
        let (_, r) = execute(&host, prompt(PermissionMode::Default)).await;
        let connected = async |host: &Host| {
            for _ in 0..200 {
                let s = host.mcp_servers(&r.session_id);
                if s.iter().all(|s| s.state == McpState::Connected) && !s.is_empty() {
                    return s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("never connected: {:?}", host.mcp_servers(&r.session_id));
        };
        let s = connected(&host).await;
        assert_eq!((s[0].name.as_str(), s[0].source.as_str(), s[0].tools.len()), ("fake", ".mcp.json", 2), "{s:?}");
        assert_eq!(s[0].tools[0].name, "echo");
        assert!(host.mcp_reconnect(&r.session_id, "fake").await);
        assert!(!host.mcp_reconnect(&r.session_id, "nope").await);
        assert_eq!(connected(&host).await.len(), 1);
        assert!(host.mcp_servers("no-such-session").is_empty());
        host.shutdown().await;
    });
}
