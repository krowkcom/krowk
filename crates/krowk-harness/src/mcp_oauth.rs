//! Signing in to a remote MCP server that asks for OAuth (R-MCP-6 to 9, 12,
//! 13). Written in krowk on the pieces of `oauth` (PKCE, the loopback
//! listener, the credentials lock), beside the xAI login rather than
//! generalising it: the endpoints here come from the server's own
//! `WWW-Authenticate` answer, RFC 9728 protected-resource metadata and RFC
//! 8414 metadata on whatever host that names, and krowk registers itself
//! (RFC 7591) as a public client every time.
//!
//! A sign-in is two steps so a front end can sit between them: `start`
//! discovers, registers and builds the page to open; `Pending::finish_loopback`
//! (the browser answers a one-shot `127.0.0.1` listener) or
//! `Pending::finish_pasted` (the person pastes the address the browser landed
//! on) checks the state, exchanges the code and stores the result. Nothing
//! is written until the tokens are in hand: a sign-in that fails or is
//! abandoned leaves the credentials file as it was.
//!
//! The sign-ins are the `mcp` section of krowk's credentials file, apart
//! from the provider logins, keyed by the server's name. An entry records
//! the server URL, so a server whose URL changed counts as signed out.
//! **The tokens are secrets**: never printed, logged or put in an error.

use crate::engine::EngineError;
use crate::oauth::{clean, fail, oauth_error, post_form, stored_from, trusted, Endpoints, Pkce, Stored, AUTH_REQUEST_TIMEOUT, LOGIN_TIMEOUT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// One server's sign-in as the credentials file keeps it: the server URL,
/// the registered client (`client_id`) and the tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub url: String,
    #[serde(flatten)]
    pub stored: Stored,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mcp: BTreeMap<String, Entry>,
    /// The provider logins, keys and the registry's, kept as found.
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn storage(e: String) -> EngineError {
    EngineError::new("credentials_unwritable", e)
}

/// The server's sign-in, when there is one for this very URL.
pub fn load(credentials: &Path, name: &str, url: &str) -> Result<Option<Stored>, EngineError> {
    let f: File = krowk_api::creds::read(credentials).map_err(storage)?;
    Ok(f.mcp.get(name).filter(|e| same_url(&e.url, url)).map(|e| e.stored.clone()))
}

/// Stores a sign-in, under the credentials file's lock.
pub fn save(credentials: &Path, name: &str, url: &str, stored: Stored) -> Result<(), EngineError> {
    krowk_api::creds::modify(credentials, |f: &mut File| {
        f.mcp.insert(name.into(), Entry { url: url.into(), stored });
        Ok(((), true))
    })
    .map_err(storage)
}

/// Forgets a server's tokens and registration. Whether there were any.
pub fn remove(credentials: &Path, name: &str) -> Result<bool, EngineError> {
    krowk_api::creds::modify(credentials, |f: &mut File| {
        let had = f.mcp.remove(name).is_some();
        Ok((had, had))
    })
    .map_err(storage)
}

/// `https(what, url)`: the endpoint, or the failed check by name.
fn https(what: &str, url: &str) -> Result<url::Url, EngineError> {
    trusted(url).map_err(|_| fail(format!("the {what} {url} is not https — krowk signs in to a server only over https (the https check on every metadata, authorization and token endpoint)")))
}

/// What a sign-in in progress holds until the browser (or the person)
/// answers.
pub struct Pending {
    /// The page to open to sign in.
    pub authorize_url: String,
    server: String,
    url: String,
    endpoints: Endpoints,
    client_id: String,
    redirect: String,
    state: String,
    verifier: String,
    resource: String,
    scope: String,
    listener: tokio::net::TcpListener,
}

fn www_authenticate_metadata(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for v in headers.get_all(reqwest::header::WWW_AUTHENTICATE) {
        let Ok(v) = v.to_str() else { continue };
        if let Some(i) = v.find("resource_metadata=\"") {
            let rest = &v[i + "resource_metadata=\"".len()..];
            return rest.split('"').next().map(String::from);
        }
    }
    None
}

async fn json_at(http: &reqwest::Client, url: &str) -> Result<Option<Value>, EngineError> {
    crate::oauth::get_json(http, url).await
}

/// The server's protected-resource metadata (RFC 9728): where its 401 points,
/// else the well-known path under its own host.
async fn resource_metadata(http: &reqwest::Client, server: &str, url: &str) -> Result<Value, EngineError> {
    let u = https("server URL", url)?;
    let probe = http
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .timeout(AUTH_REQUEST_TIMEOUT)
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "krowk", "version": env!("CARGO_PKG_VERSION")}}}).to_string())
        .send()
        .await
        .map_err(|e| fail(format!("{server} ({url}) could not be reached: {e}")))?;
    if probe.status().as_u16() != 401 {
        return Err(fail(format!("{server} did not ask for a sign-in (HTTP {}) — it needs none, or takes a header in its config", probe.status().as_u16())));
    }
    let candidate = match www_authenticate_metadata(probe.headers()) {
        Some(m) => m,
        None => format!("{}://{}/.well-known/oauth-protected-resource{}", u.scheme(), u.host_str().map(|h| u.port().map_or(h.to_string(), |p| format!("{h}:{p}"))).unwrap_or_default(), u.path().trim_end_matches('/')),
    };
    https("protected-resource metadata", &candidate)?;
    json_at(http, &candidate).await?.ok_or_else(|| fail(format!("{server}'s protected-resource metadata at {candidate} could not be read")))
}

/// The authorization server's metadata (RFC 8414: the path-inserted form
/// first, then appended; then OpenID discovery), on whatever host it lives.
async fn server_metadata(http: &reqwest::Client, issuer: &str) -> Result<(Endpoints, bool), EngineError> {
    let iss = issuer.trim_end_matches('/');
    let u = https("authorization server", iss)?;
    let mut o = u.clone();
    o.set_path("");
    o.set_query(None);
    let origin = o.as_str().trim_end_matches('/').to_string();
    let path = u.path().trim_end_matches('/');
    let mut candidates = vec![format!("{origin}/.well-known/oauth-authorization-server{path}"), format!("{iss}/.well-known/oauth-authorization-server"), format!("{iss}/.well-known/openid-configuration")];
    candidates.dedup();
    let mut meta = None;
    for c in &candidates {
        if let Some(m) = json_at(http, c).await? {
            meta = Some(m);
            break;
        }
    }
    let meta = meta.ok_or_else(|| fail(format!("{iss} publishes no OAuth metadata (tried {})", candidates.join(", "))))?;
    let s = |k: &str| meta.get(k).and_then(Value::as_str).map(String::from);
    let named = s("issuer").unwrap_or_default();
    if named.trim_end_matches('/') != iss {
        return Err(fail(format!("{iss}'s metadata names the issuer {named:?}, not itself — refusing to sign in against it (issuer check)")));
    }
    if !meta.get("code_challenge_methods_supported").and_then(Value::as_array).is_some_and(|m| m.iter().any(|v| v == "S256")) {
        return Err(fail(format!("{iss} does not advertise PKCE S256, which krowk requires (PKCE check)")));
    }
    let token = s("token_endpoint").ok_or_else(|| fail(format!("{iss}'s metadata names no token endpoint")))?;
    https("token endpoint", &token)?;
    let authorization = s("authorization_endpoint").ok_or_else(|| fail(format!("{iss}'s metadata names no authorization endpoint")))?;
    https("authorization endpoint", &authorization)?;
    let registration = s("registration_endpoint");
    if let Some(r) = &registration {
        https("registration endpoint", r)?;
    }
    let offline = meta.get("scopes_supported").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v == "offline_access"));
    Ok((Endpoints { issuer: iss.into(), authorization: Some(authorization), token, device_authorization: None, registration }, offline))
}

/// Registers krowk as a public client with the loopback redirect (RFC 7591).
async fn register(http: &reqwest::Client, server: &str, e: &Endpoints, redirect: &str) -> Result<String, EngineError> {
    let Some(reg) = &e.registration else {
        return Err(fail(format!("{server} requires a sign-in, but its authorization server ({}) offers no client registration — krowk cannot sign in to it", e.issuer)));
    };
    let body = json!({
        "client_name": "krowk",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let r = http
        .post(reg)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .timeout(AUTH_REQUEST_TIMEOUT)
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| fail(format!("{reg} could not be reached: {e}")))?;
    let status = r.status();
    let v: Value = r.text().await.ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    match v.get("client_id").and_then(Value::as_str) {
        Some(id) if status.is_success() && !id.is_empty() => Ok(id.into()),
        _ => {
            let (err, desc) = oauth_error(&v);
            Err(fail(format!("{} refused to register krowk (HTTP {}: {err} {desc})", e.issuer, status.as_u16())))
        }
    }
}

/// Begins a sign-in to the server `name` at `url`: discovers, registers,
/// and returns the page to open. Writes nothing.
pub async fn start(http: &reqwest::Client, name: &str, url: &str) -> Result<Pending, EngineError> {
    let prm = resource_metadata(http, name, url).await?;
    let resource = prm.get("resource").and_then(Value::as_str).unwrap_or_default().to_string();
    if !same_url(&resource, url) {
        return Err(fail(format!("{name}'s metadata names the resource {resource:?}, not {url} — refusing to sign in (resource check)")));
    }
    let issuer = prm
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .ok_or_else(|| fail(format!("{name}'s protected-resource metadata names no authorization server")))?
        .to_string();
    let (endpoints, offline) = server_metadata(http, &issuer).await?;
    let mut scopes: Vec<String> = prm.get("scopes_supported").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    if offline && !scopes.iter().any(|s| s == "offline_access") {
        scopes.push("offline_access".into());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| fail(format!("no loopback port for the sign-in answer: {e}")))?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| fail(e.to_string()))?.port());
    let client_id = register(http, name, &endpoints, &redirect).await?;
    let pkce = Pkce::new()?;
    let state = crate::oauth::random_token(24)?;
    let mut page = https("authorization endpoint", endpoints.authorization.as_deref().unwrap_or_default())?;
    let scope = scopes.join(" ");
    {
        let mut q = page.query_pairs_mut();
        q.append_pair("response_type", "code").append_pair("client_id", &client_id).append_pair("redirect_uri", &redirect).append_pair("state", &state).append_pair("code_challenge", &pkce.challenge).append_pair("code_challenge_method", "S256").append_pair("resource", &resource);
        if !scope.is_empty() {
            q.append_pair("scope", &scope);
        }
    }
    Ok(Pending { authorize_url: page.into(), server: name.into(), url: url.into(), endpoints, client_id, redirect, state, verifier: pkce.verifier, resource, scope, listener })
}

impl Pending {
    /// Waits for the browser's redirect on the loopback listener (ten
    /// minutes at most), then finishes.
    pub async fn finish_loopback(self, http: &reqwest::Client, credentials: &Path) -> Result<(), EngineError> {
        let code = tokio::time::timeout(LOGIN_TIMEOUT, crate::oauth::callback(&self.listener, &self.state))
            .await
            .map_err(|_| fail("no answer from the browser within ten minutes — run the login again, or use --no-browser"))??;
        self.exchange(http, credentials, &code).await
    }

    /// Finishes with the address the browser landed on, pasted: its state
    /// is checked before its code is used.
    pub async fn finish_pasted(self, http: &reqwest::Client, credentials: &Path, landing: &str) -> Result<(), EngineError> {
        let u = url::Url::parse(landing.trim()).map_err(|_| fail("that is not the address the browser landed on — paste the whole address, from http:// on"))?;
        let q: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        if q.get("state").map(String::as_str) != Some(self.state.as_str()) {
            return Err(fail("the pasted address carries another state than this sign-in's — refused (state check); start again"));
        }
        if let Some(err) = q.get("error") {
            return Err(fail(format!("the sign-in was refused ({} {})", clean(err), clean(q.get("error_description").map(String::as_str).unwrap_or_default()))));
        }
        let code = q.get("code").filter(|c| !c.is_empty()).ok_or_else(|| fail("the pasted address carries no code — paste the address the browser landed on after you signed in"))?;
        self.exchange(http, credentials, code).await
    }

    async fn exchange(self, http: &reqwest::Client, credentials: &Path, code: &str) -> Result<(), EngineError> {
        let (status, v) = post_form(
            http,
            &self.endpoints.token,
            &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &self.redirect), ("client_id", &self.client_id), ("code_verifier", &self.verifier), ("resource", &self.resource)],
        )
        .await?;
        if !(200..300).contains(&status) {
            let (err, desc) = oauth_error(&v);
            return Err(fail(format!("{} refused the sign-in code (HTTP {status}: {err} {desc})", self.endpoints.issuer)));
        }
        let stored = stored_from(&v, &self.endpoints, &self.client_id, &self.scope, None)?;
        save(credentials, &self.server, &self.url, stored)
    }
}

/// How a front end hears the page to open, and (for a paste) the address.
pub struct Ui<'a> {
    /// Told the page to sign in on.
    pub tell: &'a mut dyn FnMut(&str),
    /// With none, the answer arrives on the loopback listener; with one it
    /// is called for the pasted address.
    pub paste: Option<&'a mut dyn FnMut() -> Result<String, EngineError>>,
}

/// Signs in from blocking code (`krowk mcp login`), on a runtime of its own.
pub fn sign_in(credentials: &Path, name: &str, url: &str, ui: Ui<'_>) -> Result<(), EngineError> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}")))?;
    rt.block_on(async {
        let http = crate::http::client()?;
        let pending = start(&http, name, url).await?;
        (ui.tell)(&pending.authorize_url);
        match ui.paste {
            Some(paste) => {
                let landing = paste()?;
                pending.finish_pasted(&http, credentials, &landing).await
            }
            None => pending.finish_loopback(&http, credentials).await,
        }
    })
}
