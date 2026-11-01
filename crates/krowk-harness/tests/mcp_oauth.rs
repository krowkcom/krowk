//! `krowk mcp login` (R-MCP-6 to 9, 12, 13) against a fake MCP server and a
//! fake authorization server on another port, shaped like Mobbin's: a
//! path-suffixed resource_metadata, an issuer with a path whose metadata is
//! served only at the path-inserted location, registration, PKCE S256.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::mcp_oauth::{self, Ui};
use mock::{Mock, Reply, Seen};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Knobs {
    registration: bool,
    resource: Option<String>,
    token_endpoint: Option<String>,
}

struct Fake {
    mcp: Mock,
    auth: Mock,
}

fn form(raw: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect()
}

fn fake(knobs: Knobs) -> Fake {
    let (mcp_url, auth_url): (Arc<Mutex<String>>, Arc<Mutex<String>>) = Default::default();
    let (m, a) = (mcp_url.clone(), auth_url.clone());
    let mcp = mock::serve_seen(move |seen: &Seen, _| {
        let own = m.lock().unwrap().clone();
        match (seen.method.as_str(), seen.path.as_str()) {
            ("POST", "/mcp") => {
                let mut r = Reply::json(401, &json!({"error": "unauthorized"}));
                r.headers.push(("www-authenticate".into(), format!("Bearer resource_metadata=\"{own}/.well-known/oauth-protected-resource/mcp\"")));
                r
            }
            ("GET", "/.well-known/oauth-protected-resource/mcp") => Reply::json(
                200,
                &json!({"resource": knobs.resource.clone().unwrap_or(format!("{own}/mcp")), "authorization_servers": [format!("{}/auth/v1", a.lock().unwrap())], "scopes_supported": ["openid"]}),
            ),
            _ => Reply::status(404, ""),
        }
    });
    *mcp_url.lock().unwrap() = mcp.url.clone();
    let codes: Arc<Mutex<HashMap<String, String>>> = Default::default();
    let registered = Arc::new(Mutex::new(knobs.registration));
    let (b, token_endpoint) = (auth_url.clone(), knobs.token_endpoint.clone());
    let auth = mock::serve_seen(move |seen: &Seen, _| {
        let own = b.lock().unwrap().clone();
        let (path, query) = seen.path.split_once('?').unwrap_or((&seen.path, ""));
        match (seen.method.as_str(), path) {
            // Only at the path-inserted location.
            ("GET", "/.well-known/oauth-authorization-server/auth/v1") => {
                let mut meta = json!({
                    "issuer": format!("{own}/auth/v1"),
                    "authorization_endpoint": format!("{own}/authorize"),
                    "token_endpoint": token_endpoint.clone().unwrap_or(format!("{own}/token")),
                    "code_challenge_methods_supported": ["S256"],
                    "scopes_supported": ["openid", "offline_access"],
                });
                if *registered.lock().unwrap() {
                    meta["registration_endpoint"] = json!(format!("{own}/register"));
                }
                Reply::json(200, &meta)
            }
            ("POST", "/register") => Reply::json(201, &json!({"client_id": "mobbin-client"})),
            ("GET", "/authorize") => {
                let f = form(query);
                let code = format!("code-{}", codes.lock().unwrap().len() + 1);
                codes.lock().unwrap().insert(code.clone(), f["code_challenge"].clone());
                Reply::redirect(&format!("{}?code={code}&state={}", f["redirect_uri"], f["state"]))
            }
            ("POST", "/token") => {
                let f = form(&seen.raw);
                use base64::Engine as _;
                use sha2::Digest as _;
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(f.get("code_verifier").cloned().unwrap_or_default().as_bytes()));
                match f.get("code").and_then(|c| codes.lock().unwrap().remove(c)) {
                    Some(c) if c == challenge && f.contains_key("resource") => Reply::json(200, &json!({"access_token": "at-secret-1", "refresh_token": "rt-secret-1", "expires_in": 3600, "token_type": "Bearer"})),
                    _ => Reply::json(400, &json!({"error": "invalid_grant"})),
                }
            }
            _ => Reply::status(404, ""),
        }
    });
    *auth_url.lock().unwrap() = auth.url.clone();
    Fake { mcp, auth }
}

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
    assert!(std::fs::read_to_string(&path).unwrap().find("at-secret").is_none());
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
