//! Signing in to xAI with a SuperGrok (or X Premium) subscription, which
//! xAI promotes for third-party harnesses (R-PROV-4), and keeping the
//! tokens fresh.
//!
//! Standard OAuth 2.0, nothing xAI-specific in the code: the endpoints come
//! from the authorization server's own metadata (RFC 8414, or OpenID
//! discovery) under the instance's `issuer`, so a moved endpoint is a
//! metadata change, not a krowk release. Two ways in:
//!
//! - **Authorization code with PKCE** (RFC 7636), the default: a browser
//!   opens xAI's sign-in page, and the answer comes back to a one-shot
//!   listener on `127.0.0.1` (RFC 8252's loopback redirect). The `state`
//!   and the S256 challenge are fresh random values for every login.
//! - **Device code** (RFC 8628), `--device`: for a host with no browser —
//!   krowk prints a code and a URL to open anywhere, and polls.
//!
//! The client id is the instance's `clientId`. With none, krowk registers
//! itself (RFC 7591) where the server's metadata offers registration; it
//! never borrows another program's client id.
//!
//! **The tokens are secrets.** They live in krowk's provider credentials
//! file (`providers/credentials.json` in krowk's config directory), created
//! `0600` in a `0700` directory and replaced by rename, never in place; no
//! token is ever printed, logged or put in an error. The file is separate
//! from the registry's `credentials.json`, whose writer owns every byte of
//! it, so a registry login can never drop an xAI refresh token and a token
//! refresh can never touch a registry key — and it keeps the name
//! `credentials.json`, which `krowk_push` refuses to publish wherever it
//! sits. xAI rotates refresh tokens, so a refresh
//! holds a lock on the file and re-reads it first: two krowk processes
//! refreshing at once would otherwise each spend the other's token.

use crate::engine::EngineError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The file's place in krowk's config directory: `providers/credentials.json`.
pub const CREDENTIALS_FILE: &str = "providers/credentials.json";
/// A token this close to expiring is refreshed before it is used.
const EXPIRY_MARGIN_MS: i64 = 60_000;
/// How long a login waits for the browser, or the device code, at most.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// A request to the authorization server — metadata, a token — answers
/// within this or fails: the client's own read timeout is the five minutes
/// a model stream may sit silent, far too long for a token.
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a refresh waits for another krowk's refresh to finish.
const LOCK_WAIT: Duration = Duration::from_secs(45);
const LOCK_POLL: Duration = Duration::from_millis(50);

fn fail(message: impl Into<String>) -> EngineError {
    EngineError::new("oauth_failed", message)
}

fn now_ms() -> i64 {
    krowk_store::now_ms()
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
    fn fresh(&self, at_ms: i64) -> bool {
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
    /// Whatever a later krowk writes that this one does not know, kept.
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

/// krowk's provider credentials file.
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
        match std::fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| fail(format!("{} is not a credentials file krowk can read ({e}) — refusing to write over it; move it aside and sign in again", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File { version: 1, ..File::default() }),
            Err(e) => Err(fail(format!("{} cannot be read: {e}", self.path.display()))),
        }
    }

    pub fn load(&self, instance: &str) -> Result<Option<Stored>, EngineError> {
        Ok(self.read()?.instances.remove(instance))
    }

    /// Stores a login, under the store's lock: a login and a refresh in
    /// another krowk never interleave their read and their write.
    pub fn save(&self, instance: &str, stored: &Stored) -> Result<(), EngineError> {
        let _lock = self.lock_blocking()?;
        self.save_locked(instance, stored)
    }

    /// `save`, for a caller already holding the lock.
    fn save_locked(&self, instance: &str, stored: &Stored) -> Result<(), EngineError> {
        let mut f = self.read()?;
        f.version = 1;
        f.instances.insert(instance.into(), stored.clone());
        self.write(&f)
    }

    /// Forgets an instance's login, under the store's lock. Whether there
    /// was one.
    pub fn remove(&self, instance: &str) -> Result<bool, EngineError> {
        let _lock = self.lock_blocking()?;
        let mut f = self.read()?;
        let had = f.instances.remove(instance).is_some();
        if had {
            self.write(&f)?;
        }
        Ok(had)
    }

    /// Every instance with a login.
    pub fn names(&self) -> Result<Vec<String>, EngineError> {
        Ok(self.read()?.instances.into_keys().collect())
    }

    fn write(&self, f: &File) -> Result<(), EngineError> {
        let io = |e: std::io::Error| fail(format!("{} could not be written: {e}", self.path.display()));
        let dir = self.path.parent().unwrap_or(Path::new("."));
        private_dir(dir).map_err(io)?;
        let data = serde_json::to_string_pretty(f).expect("credentials serialize") + "\n";
        let tmp = dir.join(format!(".credentials.json.{}.{}", std::process::id(), random_token(6)?));
        let result = (|| {
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let mut file = o.open(&tmp)?;
            file.write_all(data.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(io)
    }

    /// The store's lock, if nobody holds it: one writer at a time across
    /// every krowk on the host. Never blocks — the async side polls it, so a
    /// runtime waiting on another krowk's refresh still hears Ctrl-C.
    fn try_lock(&self) -> Result<Option<Lock>, EngineError> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        private_dir(dir).map_err(|e| fail(format!("{} could not be created: {e}", dir.display())))?;
        let path = self.path.with_extension("lock");
        let mut o = std::fs::OpenOptions::new();
        o.create(true).truncate(false).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let file = o.open(&path).map_err(|e| fail(format!("{} could not be opened: {e}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock on a descriptor this function owns; released
            // when the file is closed.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(fail(format!("{} could not be locked: {e}", path.display())));
            }
        }
        Ok(Some(Lock { _file: file }))
    }

    fn busy(&self) -> EngineError {
        fail(format!("another krowk held {} for {} seconds — run the command again", self.path.display(), LOCK_WAIT.as_secs()))
    }

    /// The lock, waited for — from blocking code: a login or a remove.
    fn lock_blocking(&self) -> Result<Lock, EngineError> {
        let until = std::time::Instant::now() + LOCK_WAIT;
        loop {
            if let Some(l) = self.try_lock()? {
                return Ok(l);
            }
            if std::time::Instant::now() > until {
                return Err(self.busy());
            }
            std::thread::sleep(LOCK_POLL);
        }
    }

    /// The lock, waited for on the runtime without blocking it.
    async fn lock_async(&self) -> Result<Lock, EngineError> {
        let until = tokio::time::Instant::now() + LOCK_WAIT;
        loop {
            if let Some(l) = self.try_lock()? {
                return Ok(l);
            }
            if tokio::time::Instant::now() > until {
                return Err(self.busy());
            }
            tokio::time::sleep(LOCK_POLL).await;
        }
    }
}

struct Lock {
    _file: std::fs::File,
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// `n` random bytes from the operating system, URL-safe base64. An OS that
/// cannot give them fails the login rather than signing in with a
/// guessable verifier.
fn random_token(n: usize) -> Result<String, EngineError> {
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

/// An endpoint krowk will send a secret to: https, or http on a loopback
/// address (a local stand-in).
fn trusted(url: &str) -> Result<url::Url, EngineError> {
    let u = url::Url::parse(url).map_err(|e| fail(format!("{url:?} is not a URL ({e})")))?;
    let loopback = matches!(u.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback()) || matches!(u.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback()) || u.host_str() == Some("localhost");
    if u.scheme() == "https" || (u.scheme() == "http" && loopback) {
        Ok(u)
    } else {
        Err(fail(format!("{url} is not https — krowk sends credentials to https endpoints only")))
    }
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Option<Value>, EngineError> {
    let r = http.get(url).header("accept", "application/json").timeout(AUTH_REQUEST_TIMEOUT).send().await.map_err(|e| fail(format!("{url} could not be reached: {e}")))?;
    if !r.status().is_success() {
        return Ok(None);
    }
    Ok(r.text().await.ok().and_then(|t| serde_json::from_str(&t).ok()))
}

/// The server's endpoints: RFC 8414 metadata, else OpenID discovery.
pub async fn discover(http: &reqwest::Client, issuer: &str) -> Result<Endpoints, EngineError> {
    let issuer = issuer.trim_end_matches('/');
    trusted(issuer)?;
    let mut meta = None;
    for well_known in ["/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"] {
        if let Some(m) = get_json(http, &format!("{issuer}{well_known}")).await? {
            meta = Some(m);
            break;
        }
    }
    let meta = meta.ok_or_else(|| fail(format!("{issuer} publishes no OAuth metadata (/.well-known/oauth-authorization-server or /.well-known/openid-configuration) — check the instance's issuer")))?;
    let s = |k: &str| meta.get(k).and_then(Value::as_str).map(String::from);
    // RFC 8414 §3.3: the metadata must name the issuer it was fetched for,
    // or it is someone else's, and its endpoints are not to be trusted
    // with a login.
    let named = s("issuer").unwrap_or_default();
    if named.trim_end_matches('/') != issuer {
        return Err(fail(format!("{issuer}'s metadata names the issuer {named:?}, not itself — refusing to sign in against it; check the instance's issuer")));
    }
    let token = s("token_endpoint").ok_or_else(|| fail(format!("{issuer}'s metadata names no token endpoint")))?;
    trusted(&token)?;
    let e = Endpoints { issuer: issuer.into(), authorization: s("authorization_endpoint"), token, device_authorization: s("device_authorization_endpoint"), registration: s("registration_endpoint") };
    for u in [&e.authorization, &e.device_authorization, &e.registration].into_iter().flatten() {
        trusted(u)?;
    }
    Ok(e)
}

/// An OAuth error answer's words: `error` and `error_description`.
fn oauth_error(v: &Value) -> (String, String) {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    (s("error"), s("error_description"))
}

async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), EngineError> {
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
fn stored_from(v: &Value, e: &Endpoints, client_id: &str, scope: &str, old_refresh: Option<String>) -> Result<Stored, EngineError> {
    let access = v.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| fail("the token endpoint answered without an access token"))?;
    let now = now_ms();
    Ok(Stored {
        issuer: e.issuer.clone(),
        client_id: client_id.into(),
        token_endpoint: e.token.clone(),
        access_token: access.into(),
        // A server that does not rotate the refresh token keeps the old one.
        refresh_token: v.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(String::from).or(old_refresh),
        expires_at_ms: v.get("expires_in").and_then(Value::as_i64).map(|s| now + s * 1000),
        scope: v.get("scope").and_then(Value::as_str).unwrap_or(scope).into(),
        obtained_at_ms: now,
    })
}

/// What a login needs.
#[derive(Debug, Clone)]
pub struct Login {
    pub issuer: String,
    pub client_id: Option<String>,
    pub scope: String,
}

/// The client id to sign in as: the configured one, else one registered
/// for krowk where the server offers registration.
async fn client_id(http: &reqwest::Client, login: &Login, e: &Endpoints, redirect_uri: Option<&str>) -> Result<String, EngineError> {
    if let Some(id) = &login.client_id {
        return Ok(id.clone());
    }
    let Some(reg) = &e.registration else {
        return Err(fail(format!(
            "{} offers no client registration, and the instance names no clientId — pass --client-id with the id xAI issued for krowk",
            e.issuer
        )));
    };
    let mut body = serde_json::json!({
        "client_name": "krowk",
        "grant_types": ["authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": login.scope,
    });
    if let Some(r) = redirect_uri {
        body["redirect_uris"] = serde_json::json!([r]);
    }
    let r = http.post(reg).header("content-type", "application/json").body(body.to_string()).send().await.map_err(|e| fail(format!("{reg} could not be reached: {e}")))?;
    let status = r.status();
    let v: Value = r.text().await.ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    match v.get("client_id").and_then(Value::as_str) {
        Some(id) if status.is_success() && !id.is_empty() => Ok(id.into()),
        _ => {
            let (err, desc) = oauth_error(&v);
            Err(fail(format!("{} refused to register krowk (HTTP {}: {err} {desc}) — pass --client-id", e.issuer, status.as_u16())))
        }
    }
}

/// What a person must do to finish a device login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePrompt {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
}

/// Signs in with the device code flow (RFC 8628): `show` is told the code
/// and where to enter it, then the token endpoint is polled until the
/// person approves, refuses, or the code expires.
pub async fn login_device(http: &reqwest::Client, login: &Login, show: &mut dyn FnMut(&DevicePrompt)) -> Result<Stored, EngineError> {
    let e = discover(http, &login.issuer).await?;
    let device = e.device_authorization.clone().ok_or_else(|| fail(format!("{} offers no device login — sign in without --device", e.issuer)))?;
    let client = client_id(http, login, &e, None).await?;
    let (status, v) = post_form(http, &device, &[("client_id", &client), ("scope", &login.scope)]).await?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let (Some(device_code), Some(user_code), Some(uri)) = (s("device_code"), s("user_code"), s("verification_uri").or_else(|| s("verification_url"))) else {
        let (err, desc) = oauth_error(&v);
        return Err(fail(format!("{} refused the device login (HTTP {status}: {err} {desc})", e.issuer)));
    };
    show(&DevicePrompt { user_code, verification_uri: uri, verification_uri_complete: s("verification_uri_complete") });
    let mut interval = Duration::from_secs(v.get("interval").and_then(Value::as_u64).unwrap_or(5).clamp(1, 60));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(v.get("expires_in").and_then(Value::as_u64).unwrap_or(900)).min(LOGIN_TIMEOUT);
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() > deadline {
            return Err(fail("the device code expired before the sign-in was approved — run the login again"));
        }
        let (status, v) = post_form(http, &e.token, &[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", &device_code), ("client_id", &client)]).await?;
        if (200..300).contains(&status) {
            return stored_from(&v, &e, &client, &login.scope, None);
        }
        match oauth_error(&v) {
            (err, _) if err == "authorization_pending" => {}
            (err, _) if err == "slow_down" => interval += Duration::from_secs(5),
            (err, _) if err == "access_denied" => return Err(fail("the sign-in was refused in the browser")),
            (err, _) if err == "expired_token" => return Err(fail("the device code expired before the sign-in was approved — run the login again")),
            (err, desc) => return Err(fail(format!("{} refused the device login (HTTP {status}: {err} {desc})", e.issuer))),
        }
    }
}

/// Signs in with an authorization code and PKCE, the answer arriving on a
/// one-shot loopback listener. `open` is handed the sign-in URL — to open
/// in a browser, or print.
pub async fn login_pkce(http: &reqwest::Client, login: &Login, open: &mut dyn FnMut(&str)) -> Result<Stored, EngineError> {
    let e = discover(http, &login.issuer).await?;
    let authorize = e.authorization.clone().ok_or_else(|| fail(format!("{} offers no browser sign-in — use --device", e.issuer)))?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| fail(format!("no loopback port for the sign-in answer: {e}")))?;
    let port = listener.local_addr().map_err(|e| fail(e.to_string()))?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let client = client_id(http, login, &e, Some(&redirect)).await?;
    let pkce = Pkce::new()?;
    let state = random_token(24)?;
    let mut url = trusted(&authorize)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", &login.scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    open(url.as_str());
    let code = tokio::time::timeout(LOGIN_TIMEOUT, callback(&listener, &state))
        .await
        .map_err(|_| fail("no answer from the browser within ten minutes — run the login again, or use --device"))??;
    let (status, v) = post_form(
        http,
        &e.token,
        &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect), ("client_id", &client), ("code_verifier", &pkce.verifier)],
    )
    .await?;
    if !(200..300).contains(&status) {
        let (err, desc) = oauth_error(&v);
        return Err(fail(format!("{} refused the sign-in code (HTTP {status}: {err} {desc})", e.issuer)));
    }
    stored_from(&v, &e, &client, &login.scope, None)
}

/// Waits for the browser's redirect and answers it. Anything that is not
/// the callback, or carries another state, is answered 404 and ignored: a
/// page on this machine probing the port cannot end the login.
async fn callback(listener: &tokio::net::TcpListener, state: &str) -> Result<String, EngineError> {
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
            (Some(code), None) => ("krowk is signed in. You can close this tab.", Ok(code.clone())),
            (_, err) => {
                let err = err.cloned().unwrap_or_else(|| "no code".into());
                let desc = q.get("error_description").cloned().unwrap_or_default();
                ("The sign-in did not complete. You can close this tab and look at the terminal.", Err(fail(format!("the sign-in was refused ({err} {desc})"))))
            }
        };
        let body = format!("<!doctype html><title>krowk</title><p>{page}</p>");
        let _ = conn.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        return result;
    }
}

/// The command that signs an instance in: `supergrok` is the provider's
/// own name, `supergrok:work` is `--name work`, and any other name is given
/// whole (`--name grok:team`).
pub fn login_command(instance: &str) -> String {
    match instance.strip_prefix("supergrok:") {
        _ if instance == "supergrok" => "krowk providers add supergrok".into(),
        Some(name) => format!("krowk providers add supergrok --name {name}"),
        None => format!("krowk providers add supergrok --name {instance}"),
    }
}

/// What a person is told during a login.
pub enum Step<'a> {
    /// Open this URL to sign in (PKCE).
    Open(&'a str),
    /// Enter this code at this URL (device).
    Device(&'a DevicePrompt),
}

/// Signs in, from blocking code: the CLI's `providers add supergrok`,
/// which starts a runtime of its own for it, as `krowk -p` does.
pub fn sign_in(login: &Login, device: bool, tell: &mut dyn FnMut(Step<'_>)) -> Result<Stored, EngineError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}")))?;
    rt.block_on(async {
        let http = crate::http::client()?;
        if device {
            login_device(&http, login, &mut |p: &DevicePrompt| tell(Step::Device(p))).await
        } else {
            login_pkce(&http, login, &mut |u: &str| tell(Step::Open(u))).await
        }
    })
}

/// An instance's tokens while a session uses them: the access token handed
/// out while it is fresh, refreshed when it is not.
pub struct Tokens {
    store: Store,
    instance: String,
    current: tokio::sync::Mutex<Stored>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("instance", &self.instance).finish()
    }
}

impl Tokens {
    /// The instance's login, or why there is none.
    pub fn open(store: Store, instance: &str) -> Result<Tokens, EngineError> {
        match store.load(instance)? {
            Some(s) => Ok(Tokens { store, instance: instance.into(), current: tokio::sync::Mutex::new(s) }),
            None => Err(EngineError::new(
                "not_authenticated",
                format!("the {instance} instance is not signed in — run `{}` (a SuperGrok or X Premium subscription)", login_command(instance)),
            )),
        }
    }

    /// An access token to send. `refused` says the last one was refused,
    /// so a fresh-looking token is refreshed all the same.
    pub async fn bearer(&self, http: &reqwest::Client, refused: bool) -> Result<String, EngineError> {
        let (_, never) = tokio::sync::watch::channel(false);
        Ok(self.bearer_unless(http, refused, never).await?.expect("a switch whose owner is gone never flips"))
    }

    /// `bearer`, or none when `cancel` flips while it waits — for this
    /// session's other call, or another krowk's refresh. Only the waits are
    /// given up. A refresh, once sent, runs to its end (30 s at most) and
    /// is saved: xAI rotates the refresh token as it answers, and dropping
    /// the answer would lose the only live one, and the login with it.
    pub async fn bearer_unless(&self, http: &reqwest::Client, refused: bool, mut cancel: tokio::sync::watch::Receiver<bool>) -> Result<Option<String>, EngineError> {
        let mut cur = tokio::select! {
            c = self.current.lock() => c,
            _ = crate::engine::cancelled(&mut cancel) => return Ok(None),
        };
        if !refused && cur.fresh(now_ms()) {
            return Ok(Some(cur.access_token.clone()));
        }
        let _lock = tokio::select! {
            l = self.store.lock_async() => l?,
            _ = crate::engine::cancelled(&mut cancel) => return Ok(None),
        };
        // Another krowk may have refreshed while this one waited: its token
        // is the one to use, and its refresh token the only live one.
        if let Some(disk) = self.store.load(&self.instance)?
            && disk.access_token != cur.access_token
        {
            *cur = disk;
            if cur.fresh(now_ms()) {
                return Ok(Some(cur.access_token.clone()));
            }
        }
        let refresh = cur.refresh_token.clone().ok_or_else(|| self.sign_in_again("the login has no refresh token"))?;
        trusted(&cur.token_endpoint)?;
        let (status, v) = post_form(http, &cur.token_endpoint, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &cur.client_id)]).await?;
        if !(200..300).contains(&status) {
            let (err, _) = oauth_error(&v);
            return Err(self.sign_in_again(&format!("xAI refused to refresh it (HTTP {status} {err})")).with_status(if status == 400 { 401 } else { status }));
        }
        let e = Endpoints { issuer: cur.issuer.clone(), token: cur.token_endpoint.clone(), ..Endpoints::default() };
        // Memory first: the old refresh token is spent now, and a save that
        // fails must not leave this session holding it.
        *cur = stored_from(&v, &e, &cur.client_id, &cur.scope, Some(refresh))?;
        self.store.save_locked(&self.instance, &cur)?;
        Ok(Some(cur.access_token.clone()))
    }

    fn sign_in_again(&self, why: &str) -> EngineError {
        EngineError::new("not_authenticated", format!("the {} login has expired: {why} — run `{}`", self.instance, login_command(&self.instance)))
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
        assert_eq!(login_command("supergrok"), "krowk providers add supergrok");
        assert_eq!(login_command("supergrok:work"), "krowk providers add supergrok --name work");
        assert_eq!(login_command("grok:team"), "krowk providers add supergrok --name grok:team");
    }
}
