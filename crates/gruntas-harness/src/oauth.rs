//! gruntas's provider credentials file and the OAuth pieces its sign-ins
//! share: the store, PKCE, the token request and the loopback callback.
//! `mcp_oauth` signs in to MCP servers with them; `connect` and `keys`
//! keep keys in the same file.
//!
//! **The tokens are secrets.** They live in gruntas's one credentials file
//! (`credentials.json` in gruntas's home, `gruntas_api::creds`), `0600` in the
//! `0700` home and replaced by rename, never in place; no token is ever
//! printed, logged or put in an error. The logins are its `instances`, and
//! stored API keys (`crate::keys`) its `keys`, beside the registry's keys,
//! which this reader keeps as it found them. Every write — a login, a key
//! stored or removed, a registry login — is one read-modify-write under
//! one lock (`Store::modify`, `gruntas_api::creds::modify`).

use crate::engine::EngineError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A token this close to expiring is refreshed before it is used.
const EXPIRY_MARGIN_MS: i64 = 60_000;
/// How long a login waits for the browser, or the device code, at most.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// A request to the authorization server — metadata, a token — answers
/// within this or fails: the client's own read timeout is the five minutes
/// a model stream may sit silent, far too long for a token.
pub(crate) const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn fail(message: impl Into<String>) -> EngineError {
    EngineError::new("oauth_failed", message)
}

fn now_ms() -> i64 {
    gruntas_store::now_ms()
}

/// One instance's login, as the credentials file holds it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the access token expires, in Unix milliseconds; none when the
    /// server did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    pub obtained_at_ms: i64,
}

// Hand-written so a token never reaches a log line through `{:?}`.
impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("access_token", &"<secret>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<secret>"))
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl Stored {
    pub(crate) fn fresh(&self, at_ms: i64) -> bool {
        !self.access_token.is_empty() && self.expires_at_ms.is_none_or(|e| e - EXPIRY_MARGIN_MS > at_ms)
    }

    /// A login no call can use: its access token is spent and there is no
    /// refresh token to get another, so only a new login helps. One with a
    /// refresh token is still good — the next call refreshes it.
    pub fn expired_for_good(&self, at_ms: i64) -> bool {
        !self.fresh(at_ms) && self.refresh_token.is_none()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    instances: BTreeMap<String, Stored>,
    /// Stored API keys, by instance.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    keys: BTreeMap<String, crate::keys::KeyRef>,
    /// Whatever a later gruntas writes that this one does not know, kept.
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

/// gruntas's provider credentials file.
#[derive(Debug, Clone)]
pub struct Store {
    pub path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path }
    }

    /// A file that cannot be read is an error, never an empty store:
    /// writing over it would drop every other login in it.
    fn read(&self) -> Result<File, EngineError> {
        gruntas_api::creds::read(&self.path).map_err(|e| fail(format!("{e} — refusing to write over it; fix it or move it aside and sign in again")))
    }

    pub fn load(&self, instance: &str) -> Result<Option<Stored>, EngineError> {
        Ok(self.read()?.instances.remove(instance))
    }

    /// The one way the file changes: under the store's lock, read, edit,
    /// and written back (by rename) when the edit says it changed something
    /// — so a login, a refresh and a stored key in two gruntas processes never
    /// interleave their read and their write.
    fn modify<R>(&self, edit: impl FnOnce(&mut File) -> (R, bool)) -> Result<R, EngineError> {
        let _lock = self.lock_blocking()?;
        self.modify_locked(edit)
    }

    /// `modify`, for a caller already holding the lock.
    fn modify_locked<R>(&self, edit: impl FnOnce(&mut File) -> (R, bool)) -> Result<R, EngineError> {
        let mut f = self.read()?;
        let (r, changed) = edit(&mut f);
        if changed {
            f.version = 1;
            gruntas_api::creds::write(&self.path, &f).map_err(fail)?;
        }
        Ok(r)
    }

    /// Stores a login.
    pub fn save(&self, instance: &str, stored: &Stored) -> Result<(), EngineError> {
        self.modify(|f| (f.instances.insert(instance.into(), stored.clone()), true)).map(drop)
    }

    /// Stores an API key for an instance, replacing the one it had.
    pub fn save_key(&self, instance: &str, key: &crate::keys::KeyRef) -> Result<(), EngineError> {
        self.modify(|f| (f.keys.insert(instance.into(), key.clone()), true)).map(drop)
    }

    /// Forgets an instance's login and its stored key. Whether there was
    /// either.
    pub fn remove(&self, instance: &str) -> Result<bool, EngineError> {
        self.modify(|f| {
            let had = f.instances.remove(instance).is_some() | f.keys.remove(instance).is_some();
            (had, had)
        })
    }

    /// Moves an instance's login and stored key to its new name. Whatever
    /// was left under the new name — no instance has it — goes, so nothing
    /// of another's is ever the renamed one's. Whether there was a login or
    /// a key to move.
    pub fn rename(&self, from: &str, to: &str) -> Result<bool, EngineError> {
        self.modify(|f| {
            let stale = f.instances.remove(to).is_some() | f.keys.remove(to).is_some();
            let login = f.instances.remove(from);
            let key = f.keys.remove(from);
            let had = login.is_some() || key.is_some();
            if let Some(l) = login {
                f.instances.insert(to.into(), l);
            }
            if let Some(k) = key {
                f.keys.insert(to.into(), k);
            }
            (had, had || stale)
        })
    }

    /// Every stored API key, by instance.
    pub fn keys(&self) -> Result<BTreeMap<String, crate::keys::KeyRef>, EngineError> {
        Ok(self.read()?.keys)
    }

    /// Every instance with a login.
    pub fn names(&self) -> Result<Vec<String>, EngineError> {
        Ok(self.read()?.instances.into_keys().collect())
    }

    /// The lock, waited for — from blocking code: a login or a remove.
    fn lock_blocking(&self) -> Result<gruntas_api::creds::Lock, EngineError> {
        gruntas_api::creds::lock(&self.path).map_err(fail)
    }
}

/// `n` random bytes from the operating system, URL-safe base64. An OS that
/// cannot give them fails the login rather than signing in with a
/// guessable verifier.
pub(crate) fn random_token(n: usize) -> Result<String, EngineError> {
    use base64::Engine as _;
    let mut b = vec![0u8; n];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).map_err(|_| fail("the operating system gave no random bytes for the login"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Pkce, EngineError> {
        Ok(Pkce::from_verifier(random_token(48)?))
    }

    pub fn from_verifier(verifier: String) -> Pkce {
        use base64::Engine as _;
        use sha2::Digest as _;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
        Pkce { verifier, challenge }
    }
}

/// The authorization server's endpoints, from its metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoints {
    pub issuer: String,
    pub authorization: Option<String>,
    pub token: String,
    pub device_authorization: Option<String>,
    pub registration: Option<String>,
}

/// An endpoint gruntas will send a secret to: https, or http on a loopback
/// address (a local stand-in).
pub(crate) fn trusted(url: &str) -> Result<url::Url, EngineError> {
    let u = url::Url::parse(url).map_err(|e| fail(format!("{url:?} is not a URL ({e})")))?;
    let loopback = matches!(u.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback()) || matches!(u.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback()) || u.host_str() == Some("localhost");
    if u.scheme() == "https" || (u.scheme() == "http" && loopback) {
        Ok(u)
    } else {
        Err(fail(format!("{url} is not https — gruntas sends credentials to https endpoints only")))
    }
}

pub(crate) async fn get_json(http: &reqwest::Client, url: &str) -> Result<Option<Value>, EngineError> {
    let r = http.get(url).header("accept", "application/json").timeout(AUTH_REQUEST_TIMEOUT).send().await.map_err(|e| fail(format!("{url} could not be reached: {e}")))?;
    if !r.status().is_success() {
        return Ok(None);
    }
    Ok(r.text().await.ok().and_then(|t| serde_json::from_str(&t).ok()))
}

/// Text a server sent, as one line a terminal can show: control characters
/// (an escape sequence among them) dropped.
pub(crate) fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(300).collect()
}

pub(crate) fn oauth_error(v: &Value) -> (String, String) {
    let s = |k: &str| clean(v.get(k).and_then(Value::as_str).unwrap_or_default());
    (s("error"), s("error_description"))
}

pub(crate) async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), EngineError> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let r = http
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .timeout(AUTH_REQUEST_TIMEOUT)
        .body(body)
        .send()
        .await
        .map_err(|e| fail(format!("{url} could not be reached: {e}")))?;
    let status = r.status().as_u16();
    let v = r.text().await.ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    Ok((status, v))
}

/// A token answer as what is stored.
pub(crate) fn stored_from(v: &Value, e: &Endpoints, client_id: &str, scope: &str, old_refresh: Option<String>) -> Result<Stored, EngineError> {
    let access = v.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| fail("the token endpoint answered without an access token"))?;
    let now = now_ms();
    Ok(Stored {
        issuer: e.issuer.clone(),
        client_id: client_id.into(),
        token_endpoint: e.token.clone(),
        access_token: access.into(),
        // A server that does not rotate the refresh token keeps the old one.
        refresh_token: v.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(String::from).or(old_refresh),
        expires_at_ms: v.get("expires_in").and_then(Value::as_i64).map(|s| now.saturating_add(s.saturating_mul(1000))),
        scope: v.get("scope").and_then(Value::as_str).unwrap_or(scope).into(),
        obtained_at_ms: now,
    })
}

/// Waits for the browser's redirect and answers it. Anything that is not
/// the callback, or carries another state, is answered 404 and ignored: a
/// page on this machine probing the port cannot end the login.
pub(crate) async fn callback(listener: &tokio::net::TcpListener, state: &str) -> Result<String, EngineError> {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else { continue };
        let mut buf = vec![0u8; 8192];
        let mut n = 0;
        while n < buf.len() {
            match tokio::time::timeout(Duration::from_secs(10), conn.read(&mut buf[n..])).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(k)) => {
                    n += k;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or_default().to_string();
        let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok().filter(|u| u.path() == "/callback");
        let Some(u) = parsed else {
            let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            continue;
        };
        let q: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        if q.get("state").map(String::as_str) != Some(state) {
            let _ = conn.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            continue;
        }
        let (page, result) = match (q.get("code"), q.get("error")) {
            (Some(code), None) => ("gruntas is signed in. You can close this tab.", Ok(code.clone())),
            (_, err) => {
                let err = clean(&err.cloned().unwrap_or_else(|| "no code".into()));
                let desc = clean(q.get("error_description").map(String::as_str).unwrap_or_default());
                ("The sign-in did not complete. You can close this tab and look at the terminal.", Err(fail(format!("the sign-in was refused ({err} {desc})"))))
            }
        };
        let body = format!("<!doctype html><title>gruntas</title><p>{page}</p>");
        let _ = conn.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        return result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_prov_4_pkce_is_s256_of_a_fresh_verifier() {
        // S256 = base64url(sha256(verifier)), no padding: the value Python's
        // hashlib and base64 give for this verifier.
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mJ92K1uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(p.challenge, "bE2uqzu2U3uJ17rqzvAysH4RilUGdKI9RF-LyFG35ds");
        let (a, b) = (Pkce::new().unwrap(), Pkce::new().unwrap());
        assert_ne!(a.verifier, b.verifier);
        assert!((43..=128).contains(&a.verifier.len()), "RFC 7636 bounds the verifier");
        assert!(trusted("http://example.com/token").is_err(), "a secret never goes over plain http");
        assert!(trusted("http://127.0.0.1:9/token").is_ok() && trusted("https://auth.x.ai/token").is_ok());
        let s = Stored { issuer: "i".into(), client_id: "c".into(), token_endpoint: "t".into(), access_token: "at-secret".into(), refresh_token: Some("rt-secret".into()), expires_at_ms: Some(0), scope: String::new(), obtained_at_ms: 0 };
        let shown = format!("{s:?}");
        assert!(!shown.contains("at-secret") && !shown.contains("rt-secret"), "{shown}");
        assert!(!s.fresh(1) && Stored { expires_at_ms: None, ..s.clone() }.fresh(1));
    }
}
