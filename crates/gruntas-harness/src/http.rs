//! The HTTP client gruntas makes its requests with: MCP servers and their
//! sign-ins, and sync.
//!
//! Redirects are never followed. A request carries a key or a token, and a
//! redirect is a request the client did not choose to make — to wherever
//! the answer says — with the credential attached.

use crate::engine::EngineError;
use std::sync::Arc;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A stream that sends nothing — not even a keep-alive — for this long is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// rustls on ring with the platform verifier: the same trust decisions the
/// rest of gruntas makes through ureq.
///
/// The TLS configuration is built once a process and shared: building it
/// reads and parses the platform's roots, tens of milliseconds a turn on
/// the host daemon's one thread, which every other session streams on
/// (R-LAG-9). The client itself is made per engine, on the runtime that
/// uses it, since its connection pool belongs to that runtime.
///
/// In the host daemon a connection is also made off its thread
/// (`OffThread`): the handshake checks the server's certificate
/// synchronously, and on macOS that is `SecTrustEvaluateWithError`, which
/// can take as long as the system's trust service does (R-LAG-9).
pub fn client() -> Result<reqwest::Client, EngineError> {
    let daemon = WARM_ONLY.load(std::sync::atomic::Ordering::Relaxed);
    let tls = tls(!daemon).map_err(|e| EngineError::new("tls_unavailable", format!("the TLS configuration could not be built: {e}")))?;
    let mut b = reqwest::Client::builder().use_preconfigured_tls(tls).connect_timeout(CONNECT_TIMEOUT).read_timeout(IDLE_TIMEOUT).redirect(reqwest::redirect::Policy::none());
    if daemon && let Some(io) = connect_io() {
        b = b.connector_layer(OffThread(io));
    }
    b.build().map_err(|e| EngineError::new("tls_unavailable", format!("the HTTP client could not be built: {e}")))
}

/// The runtime a connection made off the daemon's thread is registered
/// with: one thread of its own that does nothing but wait on its sockets
/// and timers, so no handshake under way ever holds up a stream already
/// open. None when its thread could not be started; connections are then
/// made where they are asked for.
fn connect_io() -> Option<tokio::runtime::Handle> {
    static IO: std::sync::OnceLock<Option<tokio::runtime::Handle>> = std::sync::OnceLock::new();
    IO.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        let handle = rt.handle().clone();
        std::thread::Builder::new().name("gruntas-connect-io".into()).spawn(move || rt.block_on(std::future::pending::<()>())).ok()?;
        Some(handle)
    })
    .clone()
}

/// Makes each connection — DNS, TCP, the TLS handshake and its certificate
/// check — on a blocking thread, its sockets on `connect_io`'s runtime,
/// rather than on the thread that asked. The timeouts and the pool are
/// reqwest's as before: the connect timeout wraps this layer, and the
/// connection, once made, is read and pooled on the asker's runtime.
#[derive(Clone)]
struct OffThread(tokio::runtime::Handle);

impl<S> tower_layer::Layer<S> for OffThread {
    type Service = OffThreadService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        OffThreadService { inner, io: self.0.clone() }
    }
}

#[derive(Clone)]
struct OffThreadService<S> {
    inner: S,
    io: tokio::runtime::Handle,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<S, R> tower_service::Service<R> for OffThreadService<S>
where
    S: tower_service::Service<R, Error = BoxError>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        // Lazy: nothing of the connection is made until it is polled, on
        // the blocking thread, inside `io`'s runtime.
        let connect = self.inner.call(req);
        let io = self.io.clone();
        // Dropped with the future that asked — the connect timeout, an
        // interrupted turn — and the connection under way is dropped with
        // it, rather than holding a blocking thread to its end.
        let (asked, gone) = tokio::sync::oneshot::channel::<()>();
        Box::pin(async move {
            let made = tokio::task::spawn_blocking(move || {
                io.block_on(async {
                    tokio::select! {
                        made = connect => Some(made),
                        _ = gone => None,
                    }
                })
            })
            .await;
            drop(asked);
            match made {
                Ok(Some(made)) => made,
                Ok(None) => Err("the connection was given up".into()),
                // A panic in the connector, said as a failed connection.
                Err(e) => Err(format!("the connection could not be made: {e}").into()),
            }
        })
    }
}

/// Only a configuration that was built is kept: a failure — the platform's
/// roots unreadable for a moment — is tried again rather than failing every
/// turn the process runs.
static TLS: std::sync::OnceLock<rustls::ClientConfig> = std::sync::OnceLock::new();

/// Held while a build runs, so builds asked for at once wait for the one
/// under way and take what it made, and the last failure with when it
/// happened: asked again within `RETRY`, the answer is that failure, not
/// another build — which the host daemon's thread would pay for.
static BUILD: std::sync::Mutex<Option<(std::time::Instant, String)>> = std::sync::Mutex::new(None);

const RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Set by the host daemon: `client` never builds the configuration, only
/// `warm` does, off the daemon's thread. On the thread a build — or a wait
/// on one under way — would stall every session; `client` there takes the
/// configuration built, or fails at once with why it is not.
static WARM_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// From here on, only `warm` builds the TLS configuration (the host
/// daemon, which calls `warm` off its thread before each turn that finds it
/// unbuilt).
pub fn warm_only() {
    WARM_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The shared configuration; built now when `build`, else only what a
/// build made, or why there is none.
fn tls(build: bool) -> Result<rustls::ClientConfig, String> {
    if let Some(tls) = TLS.get() {
        return Ok(tls.clone());
    }
    if !build {
        return match BUILD.try_lock() {
            Ok(failed) => match (TLS.get(), failed.as_ref()) {
                (Some(tls), _) => Ok(tls.clone()),
                // A failure of any age: the next turn's `warm` tries again.
                (None, Some((_, why))) => Err(why.clone()),
                (None, None) => Err("it has not been built yet".into()),
            },
            Err(std::sync::TryLockError::Poisoned(p)) => Err(p.into_inner().as_ref().map_or_else(|| "a build failed".into(), |(_, why)| why.clone())),
            Err(std::sync::TryLockError::WouldBlock) => Err("it is being built".into()),
        };
    }
    let mut failed = BUILD.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tls) = TLS.get() {
        return Ok(tls.clone());
    }
    if let Some((at, why)) = failed.as_ref()
        && at.elapsed() < RETRY
    {
        return Err(why.clone());
    }
    let built = (|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
        #[cfg(feature = "test-hooks")]
        let tls = {
            let platform = rustls_platform_verifier::Verifier::new(tls.crypto_provider().clone()).map_err(|e| e.to_string())?;
            tls.dangerous().with_custom_certificate_verifier(Arc::new(SlowVerifier(platform)))
        };
        #[cfg(not(feature = "test-hooks"))]
        let tls = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(tls).map_err(|e| e.to_string())?;
        Ok::<_, String>(tls.with_no_client_auth())
    })();
    match built {
        Ok(built) => {
            *failed = None;
            Ok(TLS.get_or_init(|| built).clone())
        }
        Err(why) => {
            *failed = Some((std::time::Instant::now(), why.clone()));
            Err(why)
        }
    }
}

/// Builds the shared TLS configuration now: the host daemon calls it off
/// its thread as it starts, and before a turn when it is not built yet, so
/// no turn pays for it there.
pub fn warm() {
    let _ = tls(true);
}

/// The shared TLS configuration, built now if it is not: what a `wss://`
/// relay link is dialed with, so the relay is trusted as the provider APIs
/// are. It may read the platform's roots, so it is called off any thread
/// that must not wait.
pub fn tls_config() -> Result<rustls::ClientConfig, String> {
    tls(true)
}

/// Whether the shared TLS configuration is built: a turn the host daemon
/// runs before its start has built it waits for `warm` off the thread
/// rather than building it on the thread (R-LAG-9).
pub fn warmed() -> bool {
    TLS.get().is_some()
}

/// For tests (`test-hooks`): how long, in µs, each check of a server's
/// certificate takes on top of the platform's own — a trust service that
/// stalls, as `SecTrustEvaluateWithError` can on macOS.
#[cfg(feature = "test-hooks")]
static SLOW_VERIFY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// For tests (`test-hooks`): makes every certificate check take `d` longer
/// until the guard goes.
#[cfg(feature = "test-hooks")]
#[must_use = "the check is slow only while the guard lives"]
pub fn simulate_slow_verify(d: Duration) -> SlowVerify {
    SLOW_VERIFY.store(d.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    SlowVerify(())
}

/// Certificate checks are slow while this lives (`simulate_slow_verify`).
#[cfg(feature = "test-hooks")]
pub struct SlowVerify(());

#[cfg(feature = "test-hooks")]
impl Drop for SlowVerify {
    fn drop(&mut self) {
        SLOW_VERIFY.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The platform verifier, its every check of a certificate first sleeping
/// `SLOW_VERIFY` on whichever thread makes it. It decides as the platform
/// does.
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
struct SlowVerifier(rustls_platform_verifier::Verifier);

#[cfg(feature = "test-hooks")]
impl rustls::client::danger::ServerCertVerifier for SlowVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        std::thread::sleep(Duration::from_micros(SLOW_VERIFY.load(std::sync::atomic::Ordering::Relaxed)));
        self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.0.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.0.root_hint_subjects()
    }
}

/// An RFC 3339 UTC time (`2026-09-26T14:00:00Z`, fractions and a numeric
/// offset allowed) in milliseconds since the epoch: when a machine's
/// presence was last updated (`crate::sync::fleet`).
pub(crate) fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(f) = rest.strip_prefix('.') {
        let digits: String = f.chars().take_while(char::is_ascii_digit).collect();
        ms = format!("{:0<3}", &digits[..digits.len().min(3)]).parse().ok()?;
        rest = &f[digits.len()..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let m = o[1..3].parse::<i64>().ok()? * 60 + o[4..6].parse::<i64>().ok()?;
            if o.starts_with('+') { m } else { -m }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset * 60) * 1000) + ms)
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}
