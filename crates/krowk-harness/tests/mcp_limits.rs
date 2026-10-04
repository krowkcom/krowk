//! What a hostile or broken MCP server cannot do to a session (R-TOOL-3):
//! fill the model's context or krowk's memory, page forever, outlive its
//! session through its children, reach the person's provider keys, or be a
//! repository's file run by a relative `command`. Each server is a bash
//! script written by the test.
#![cfg(unix)]

use krowk_harness::mcp::{self, Mcp, Server};
use krowk_harness::permissions::Config;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn base(name: &str) -> PathBuf {
    let b = std::env::temp_dir().join(format!("krowk-mcp-limits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(b.join("repo/.git")).unwrap();
    std::fs::create_dir_all(b.join("home")).unwrap();
    b.canonicalize().unwrap()
}

/// A stdio server answering `initialize`, and `tools/list` with what the
/// bash expression `list_result` prints; `before` runs first.
fn script(dir: &Path, before: &str, list_result: &str) -> PathBuf {
    let p = dir.join("srv.sh");
    let body = format!(
        r#"{before}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
  *'"method":"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}}}}}}\n' "$id" ;;
  *'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":%s}}\n' "$id" "$({list_result})" ;;
  esac
done
"#
    );
    std::fs::write(&p, body).unwrap();
    p
}

fn server(name: &str, cmd: &str, args: Vec<String>, cwd: &Path) -> Server {
    Server { name: name.into(), config: serde_json::from_value(json!({"command": cmd, "args": args})).unwrap(), source: "test".into(), cwd: cwd.into() }
}

fn search(m: &Mcp, query: &str) -> String {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (out, err) = rt.block_on(m.search(&json!({"query": query}), &|_, _| false, &rx));
    assert!(!err, "{out}");
    out
}

#[test]
fn r_tool_3_a_tool_with_a_huge_schema_is_left_out_and_a_search_stays_bounded() {
    let b = base("schema");
    // One tool whose schema holds 2 MiB, and one ordinary.
    let s = script(
        &b,
        "pad=$(head -c 2097152 /dev/zero | tr '\\0' a)",
        r#"printf '{"tools":[{"name":"huge","description":"echo","inputSchema":{"type":"object","description":"%s"}},{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}' "$pad""#,
    );
    let m = Mcp::new(vec![server("big", "bash", vec![s.display().to_string()], &b.join("home"))]);
    let out = search(&m, "echo");
    let _ = std::fs::remove_dir_all(&b);
    assert!(out.len() <= 40_000, "mcp_search returned {} bytes to the model", out.len());
    assert!(out.contains("\"big:echo\"") && !out.contains("\"big:huge\""), "{}", &out[..out.len().min(500)]);
    assert!(out.contains("input schema over 16 KiB were left out"), "{out}");
}

#[test]
fn r_tool_3_tools_list_stops_at_its_byte_cap_and_at_a_page_with_no_tools() {
    let b = base("pages");
    // Every page one 1 MiB tool, and always a next page.
    let s = script(&b, "pad=$(head -c 1048576 /dev/zero | tr '\\0' a)", r#"printf '{"tools":[{"name":"t%s","description":"%s"}],"nextCursor":"c"}' "$id" "$pad""#);
    let m = Mcp::new(vec![server("pages", "bash", vec![s.display().to_string()], &b.join("home"))]);
    let started = std::time::Instant::now();
    let out = search(&m, "nomatchword");
    assert!(started.elapsed() < Duration::from_secs(20), "listing took {:?}", started.elapsed());
    assert!(out.contains("more than 4 MiB"), "{}", &out[..out.len().min(500)]);
    assert!(out.matches("pages:t").count() <= 4, "{out}");
    // Empty pages that still name a next one: one page read, not a hundred.
    let log = b.join("pages.log");
    let s = script(&b, &format!("exec 3>>{}", log.display()), r#"echo page >&3; printf '{"tools":[],"nextCursor":"c"}'"#);
    let m = Mcp::new(vec![server("empty", "bash", vec![s.display().to_string()], &b.join("home"))]);
    search(&m, "x");
    let pages = std::fs::read_to_string(&log).unwrap().lines().count();
    let _ = std::fs::remove_dir_all(&b);
    assert_eq!(pages, 1);
}

#[test]
fn r_tool_3_a_fifo_mcp_json_does_not_hang_the_trust_probe() {
    let b = base("fifo");
    let fifo = b.join("repo/.mcp.json");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let root = b.join("repo");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(mcp::project_has_servers(&root)).unwrap());
    let got = rx.recv_timeout(Duration::from_secs(3));
    if got.is_err() {
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
    }
    let _ = std::fs::remove_dir_all(&b);
    assert_eq!(got, Ok(false));
}

#[test]
fn r_tool_3_every_spelling_of_a_whole_server_deny_starts_nothing() {
    use krowk_harness::permissions::{rules, Gate, Kind, Policy};
    let b = base("denyall");
    let marker = b.join("started");
    let s = b.join("srv.sh");
    std::fs::write(&s, format!("touch {}\n", marker.display())).unwrap();
    for rule in ["Mcp(fake)", "Mcp(fake:*)", "mcp__fake", "mcp__fake__*", "mcp__*"] {
        let _ = std::fs::remove_file(&marker);
        let mut policy = Policy::modes_only(&b.join("repo"));
        policy.loaded.rules = vec![(Kind::Deny, rules::parse(rule, "test", &b.join("repo")).unwrap())];
        let gate = Gate::new(policy, krowk_harness::protocol::PermissionMode::BypassPermissions, Default::default(), None, None, "s", "t");
        let denied = |srv: &str, tool: &str| {
            let call = krowk_harness::permissions::Call { tool: format!("mcp__{srv}__{tool}"), access: krowk_harness::permissions::Access::Mcp { server: srv.into(), tool: tool.into() }, subject: None };
            matches!(gate.verdict(&call, None), krowk_harness::permissions::Verdict::Deny(_))
        };
        let m = Mcp::new(vec![server("fake", "bash", vec![s.display().to_string()], &b.join("home"))]);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let _ = rt.block_on(m.search(&json!({"query": "x"}), &denied, &rx));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "{rule}: the server was started");
    }
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn r_tool_3_a_servers_children_go_with_it() {
    let b = base("group");
    let pid = b.join("child.pid");
    // A server that starts a long-lived child of its own, as npx does.
    let s = script(&b, &format!("sleep 300 & echo $! > {}", pid.display()), r#"printf '{"tools":[]}'"#);
    let m = Mcp::new(vec![server("grp", "bash", vec![s.display().to_string()], &b.join("home"))]);
    search(&m, "x");
    let child: i32 = std::fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
    assert_eq!(unsafe { libc::kill(child, 0) }, 0, "the child runs while the server does");
    drop(m);
    let alive = || match std::fs::read_to_string(format!("/proc/{child}/stat")) {
        // A zombie has been killed; only its parent has yet to reap it.
        Ok(st) => st.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
        Err(_) if cfg!(target_os = "linux") => false,
        Err(_) => (unsafe { libc::kill(child, 0) }) == 0,
    };
    let gone = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        !alive()
    });
    let _ = std::fs::remove_dir_all(&b);
    assert!(gone, "the server's child {child} outlived it");
}

/// The two probes that change krowk's own process — its working directory
/// and environment — in one test, so no other test in this binary sees them.
#[test]
fn r_tool_3_a_relative_command_never_runs_the_repositorys_file_and_keys_are_not_inherited() {
    let b = base("relcmd");
    let repo = b.join("repo");
    let marker = b.join("pwned");
    std::fs::create_dir_all(repo.join("bin")).unwrap();
    let evil = repo.join("bin/mcp");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&evil, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // krowk is launched in the untrusted repository.
    std::env::set_current_dir(&repo).unwrap();
    let cfg = Config {
        home: Some(b.join("home")),
        user: Some(json!({"mcpServers": {"mine": {"command": "./bin/mcp"}, "mine2": {"command": "bin/mcp"}}})),
        trusted: Some(Arc::new(|_: &Path| false)),
        ..Default::default()
    };
    let servers = mcp::discover(&cfg, &repo);
    assert!(servers.iter().all(|s| s.cwd == b.join("home")));
    search(&Mcp::new(servers), "x");
    std::thread::sleep(Duration::from_millis(300));
    std::env::set_current_dir("/").unwrap();
    assert!(!marker.exists(), "a relative command in the person's config ran the untrusted repository's file");

    // The person's provider keys stay krowk's, unless a server's own `env`
    // names one.
    // SAFETY: this binary's one test that touches the environment.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-secret");
        std::env::set_var("KROWK_TOKEN", "kr-secret");
        std::env::set_var("OPENAI_API_KEY", "sk-openai");
    }
    let dump = b.join("env.txt");
    let s = script(&b, &format!("env > {}", dump.display()), r#"printf '{"tools":[]}'"#);
    let mut srv = server("envs", "bash", vec![s.display().to_string()], &b.join("home"));
    srv.config.env.insert("OPENAI_API_KEY".into(), "sk-for-this-server".into());
    search(&Mcp::new(vec![srv]), "x");
    let env = std::fs::read_to_string(&dump).unwrap();
    let _ = std::fs::remove_dir_all(&b);
    assert!(!env.contains("sk-secret") && !env.contains("kr-secret"), "{env}");
    assert!(env.contains("OPENAI_API_KEY=sk-for-this-server"), "a key the server's config sets is its own");
}
