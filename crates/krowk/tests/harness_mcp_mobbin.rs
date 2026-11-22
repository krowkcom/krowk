//! MCP, the path a person takes with Mobbin, through the built `krowk`: it
//! adds the server and signs in by pasting the address the browser lands
//! on, and the token is stored. Against Mobbin-shaped fakes. Using the
//! server in a turn, reusing its connection and refreshing its token are
//! the host's (`krowk-harness/tests/mcp_e2e.rs`, `mcp_oauth.rs`).

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mobbin.rs"]
mod mobbin;
#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use serde_json::Value;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

fn krowk(root: &std::path::Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
    c.args(args).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", root.join("home")).env("KROWK_NO_UPDATE_CHECK", "1").current_dir(root.join("repo"));
    c
}

#[test]
fn mobbin_add_and_login_with_a_pasted_address_store_its_token() {
    let f = mobbin::fake(mobbin::Knobs { registration: true, ..Default::default() });
    let root = std::env::temp_dir().join(format!("krowk-mcp-mobbin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    let root = root.canonicalize().unwrap();
    let url = format!("{}/mcp", f.mcp.url);

    // krowk mcp add, then krowk mcp login --no-browser with the address pasted.
    let out = krowk(&root, &["mcp", "add", "mobbin", &url, "--header", "X-Client: krowk-test"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let config: Value = serde_json::from_slice(&std::fs::read(root.join("home/.krowk/config.json")).unwrap()).unwrap();
    assert_eq!(config["mcpServers"]["mobbin"]["headers"]["X-Client"], "krowk-test");
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
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&creds).unwrap()).unwrap();
    assert_eq!(stored["mcp"]["mobbin"]["accessToken"], "at-secret-1");
    assert!(!format!("{}{rest}", String::from_utf8_lossy(&out.stdout)).contains("at-secret-1"), "the token is never printed");
    let _ = std::fs::remove_dir_all(&root);
}
