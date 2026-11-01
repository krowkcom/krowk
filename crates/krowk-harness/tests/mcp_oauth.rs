//! `krowk mcp login` (R-MCP-6 to 9, 12, 13) against a fake MCP server and a
//! fake authorization server on another port, shaped like Mobbin's: a
//! path-suffixed resource_metadata, an issuer with a path whose metadata is
//! served only at the path-inserted location, registration, PKCE S256.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/mobbin.rs"]
mod mobbin;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::mcp_oauth::{self, Ui};
use mobbin::{fake, form, Fake, Knobs};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn creds(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("krowk-mcp-oauth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("credentials.json")
}

fn login(f: &Fake, path: &std::path::Path, paste: Option<&dyn Fn(&str) -> String>) -> Result<String, String> {
    let url = format!("{}/mcp", f.mcp.url);
    let (path, name) = (path.to_path_buf(), "mobbin".to_string());
    let page = Arc::new(Mutex::new(String::new()));
    let seen = page.clone();
    let result = match paste {
        None => {
            let mut tell = |u: &str| {
                *seen.lock().unwrap() = u.to_string();
                let u = u.to_string();
                std::thread::spawn(move || providers::browse(&u));
            };
            mcp_oauth::sign_in(&path, &name, &url, Ui { tell: &mut tell, paste: None })
        }
        Some(landing) => {
            let mut tell = |u: &str| *seen.lock().unwrap() = u.to_string();
            let mut read = || Ok(landing(&seen.lock().unwrap()));
            mcp_oauth::sign_in(&path, &name, &url, Ui { tell: &mut tell, paste: Some(&mut read) })
        }
    };
    let page = page.lock().unwrap().clone();
    result.map(|_| page).map_err(|e| e.message)
}

#[cfg(unix)]
fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn query(page: &str) -> HashMap<String, String> {
    form(page.split_once('?').unwrap().1)
}

#[test]
fn r_mcp_6_7_8_9_a_loopback_sign_in_registers_a_public_client_and_stores_0600() {
    let f = fake(Knobs { registration: true, ..Knobs::default() });
    let path = creds("loopback");
    std::fs::write(&path, r#"{"version":1,"instances":{"grok":{"issuer":"i","clientId":"c","tokenEndpoint":"t","accessToken":"keep-me","obtainedAtMs":1}}}"#).unwrap();
    let page = login(&f, &path, None).unwrap();
    let q = query(&page);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["resource"], format!("{}/mcp", f.mcp.url));
    assert_eq!(q["scope"], "openid offline_access");
    assert!(!q["state"].is_empty() && q["redirect_uri"].starts_with("http://127.0.0.1:"));
    let seen = f.auth.seen.lock().unwrap();
    let reg = seen.iter().find(|s| s.path == "/register").unwrap();
    assert_eq!(reg.body["token_endpoint_auth_method"], "none");
    assert_eq!(reg.body["grant_types"], json!(["authorization_code", "refresh_token"]));
    let raw = std::fs::read_to_string(&path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["mcp"]["mobbin"]["url"], format!("{}/mcp", f.mcp.url));
    assert_eq!(v["mcp"]["mobbin"]["clientId"], "mobbin-client");
    assert_eq!(v["mcp"]["mobbin"]["accessToken"], "at-secret-1");
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
    // Signed in for this URL only; logout removes it.
    assert!(mcp_oauth::load(&path, "mobbin", &format!("{}/mcp", f.mcp.url)).unwrap().is_some());
    assert!(mcp_oauth::load(&path, "mobbin", "https://elsewhere.example/mcp").unwrap().is_none());
    assert!(mcp_oauth::remove(&path, "mobbin").unwrap());
    assert!(!mcp_oauth::remove(&path, "mobbin").unwrap());
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.find("at-secret").is_none() && after.contains("keep-me"), "other sections survive a sign-in and a logout: {after}");
}

#[test]
fn r_mcp_8_13_a_pasted_address_is_state_checked_and_a_refused_one_writes_nothing() {
    let f = fake(Knobs { registration: true, ..Knobs::default() });
    let path = creds("paste");
    let e = login(&f, &path, Some(&|page: &str| {
        let q = query(page);
        format!("{}?code=forged&state=wrong", q["redirect_uri"])
    }))
    .unwrap_err();
    assert!(e.contains("state"), "{e}");
    assert!(!path.exists(), "an abandoned sign-in writes nothing");
    // The real address: the authorization server's redirect, followed by hand.
    login(&f, &path, Some(&|page: &str| {
        let q = query(page);
        let code = {
            let mut c = std::net::TcpStream::connect(f.auth.url.trim_start_matches("http://")).unwrap();
            use std::io::{Read, Write};
            write!(c, "GET /authorize?{} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n", page.split_once('?').unwrap().1).unwrap();
            let mut out = String::new();
            let _ = c.read_to_string(&mut out);
            out.lines().find_map(|l| l.strip_prefix("location: ")).unwrap().to_string()
        };
        assert!(code.contains(&q["state"]));
        code
    }))
    .unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains("rt-secret-1"));
}

#[test]
fn r_mcp_6_13_failed_checks_name_themselves_and_write_nothing_and_no_token_leaks() {
    let path = creds("checks");
    let no_registration = fake(Knobs::default());
    let e = login(&no_registration, &path, None).unwrap_err();
    assert!(e.contains("offers no client registration"), "{e}");
    let wrong_resource = fake(Knobs { registration: true, resource: Some("https://other.example/mcp".into()), ..Knobs::default() });
    let e = login(&wrong_resource, &path, None).unwrap_err();
    assert!(e.contains("resource check"), "{e}");
    let plain_token = fake(Knobs { registration: true, token_endpoint: Some("http://auth.example/token".into()), ..Knobs::default() });
    let e = login(&plain_token, &path, None).unwrap_err();
    assert!(e.contains("https check") && e.contains("token endpoint"), "{e}");
    assert!(!path.exists());
}
