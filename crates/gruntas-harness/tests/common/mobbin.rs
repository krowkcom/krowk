//! A Mobbin-shaped pair of fakes, shared by the sign-in and end-to-end
//! tests: an MCP server that answers 401 with a path-suffixed
//! resource_metadata until it is given the current bearer (then lists and
//! calls `search_screens`, which returns text and an image), and an
//! authorization server on another port whose issuer has a path, whose
//! metadata is served only at the RFC 8414 path-inserted location, with
//! registration, PKCE S256 and rotating tokens: the first sign-in issues
//! `at-secret-1` / `rt-secret-1`, each refresh the next pair.
#![allow(dead_code)]

use crate::mock::{self, Mock, Reply, Seen};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A 1x1 PNG, what `search_screens` returns as its preview.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

#[derive(Default)]
pub struct Knobs {
    pub registration: bool,
    pub resource: Option<String>,
    pub token_endpoint: Option<String>,
}

pub struct Fake {
    pub mcp: Mock,
    pub auth: Mock,
    /// The access and refresh token the servers take now.
    pub valid: Arc<Mutex<(String, String)>>,
}

pub fn form(raw: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect()
}

pub fn fake(knobs: Knobs) -> Fake {
    let (mcp_url, auth_url): (Arc<Mutex<String>>, Arc<Mutex<String>>) = Default::default();
    let (m, a) = (mcp_url.clone(), auth_url.clone());
    let valid: Arc<Mutex<(String, String)>> = Default::default();
    let (bearer, issued) = (valid.clone(), Arc::new(Mutex::new(0u32)));
    let mcp = mock::serve_seen(move |seen: &Seen, _| {
        let own = m.lock().unwrap().clone();
        match (seen.method.as_str(), seen.path.as_str()) {
            ("POST", "/mcp") => {
                let current = bearer.lock().unwrap().0.clone();
                if !current.is_empty() && seen.header("authorization") == Some(&format!("Bearer {current}")) {
                    let id = seen.body["id"].clone();
                    let result = match seen.body["method"].as_str() {
                        Some("initialize") => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "mobbin", "version": "1"}}),
                        Some("tools/list") => json!({"tools": [{"name": "search_screens", "description": "Search Mobbin's app screens.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}}]}),
                        Some("tools/call") => {
                            use base64::Engine as _;
                            json!({"content": [{"type": "text", "text": "2 screens"}, {"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(PNG), "mimeType": "image/png"}]})
                        }
                        _ => return Reply::status(202, ""),
                    };
                    return Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
                }
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
    let (b, token_endpoint, current) = (auth_url.clone(), knobs.token_endpoint.clone(), valid.clone());
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
                let mut n = issued.lock().unwrap();
                let grant = match f.get("grant_type").map(String::as_str) {
                    Some("refresh_token") => f.get("refresh_token").is_some_and(|t| *t == current.lock().unwrap().1) && f.contains_key("resource") && f.contains_key("client_id"),
                    _ => matches!(f.get("code").and_then(|c| codes.lock().unwrap().remove(c)), Some(c) if c == challenge) && f.contains_key("resource"),
                };
                if !grant {
                    return Reply::json(400, &json!({"error": "invalid_grant"}));
                }
                *n += 1;
                *current.lock().unwrap() = (format!("at-secret-{n}"), format!("rt-secret-{n}"));
                let v = current.lock().unwrap();
                Reply::json(200, &json!({"access_token": v.0, "refresh_token": v.1, "expires_in": 3600, "token_type": "Bearer"}))
            }
            _ => Reply::status(404, ""),
        }
    });
    *auth_url.lock().unwrap() = auth.url.clone();
    Fake { mcp, auth, valid }
}


/// What the browser does after approving: follow the authorization
/// server's redirect by hand and give back the address it lands on.
pub fn land(auth_url: &str, page: &str) -> String {
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(auth_url.trim_start_matches("http://")).unwrap();
    write!(c, "GET /authorize?{} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n", page.split_once('?').unwrap().1).unwrap();
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    out.lines().find_map(|l| l.strip_prefix("location: ")).unwrap().trim().to_string()
}
