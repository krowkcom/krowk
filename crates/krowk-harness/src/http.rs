//! What every wire client shares: the HTTP client, the retry rule, how an
//! error answer and a dead connection are told apart and worded, and the
//! loop that reads a stream of server-sent events into a decoder.
//!
//! Redirects are never followed. A request carries a key or a token, and a
//! redirect is a request the client did not choose to make — to wherever
//! the answer says — with the credential attached.

use crate::engine::{EngineError, EngineEvent, Events};
use crate::protocol::{LimitState, LimitStatus};
use crate::native::ModelResponse;
use crate::sse::{SseEvent, SseParser};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Attempts per call when the API answers busy (429, 5xx, 529) or cannot be
/// reached, before any of the response has streamed.
pub const ATTEMPTS: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A stream that sends nothing — not even a keep-alive — for this long is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// rustls on ring with the platform verifier: the same trust decisions the
/// rest of krowk makes through ureq.
///
/// The TLS configuration is built once a process and shared: building it
/// reads and parses the platform's roots, tens of milliseconds a turn on
/// the host daemon's one thread, which every other session streams on
/// (R-LAG-9). The client itself is made per engine, on the runtime that
/// uses it, since its connection pool belongs to that runtime.
pub fn client() -> Result<reqwest::Client, EngineError> {
    let tls = tls(!WARM_ONLY.load(std::sync::atomic::Ordering::Relaxed))
        .map_err(|e| EngineError::new("tls_unavailable", format!("the TLS configuration could not be built: {e}")))?;
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| EngineError::new("tls_unavailable", format!("the HTTP client could not be built: {e}")))
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
        Ok::<_, String>(rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(tls).map_err(|e| e.to_string())?.with_no_client_auth())
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

/// Whether the shared TLS configuration is built: a turn the host daemon
/// runs before its start has built it waits for `warm` off the thread
/// rather than building it on the thread (R-LAG-9).
pub fn warmed() -> bool {
    TLS.get().is_some()
}

/// Who a request went to, for the words of a failure.
#[derive(Debug, Clone, Copy)]
pub struct Peer<'a> {
    /// The provider as a person names it: `Anthropic`, `OpenAI`, `xAI`.
    pub vendor: &'a str,
    pub instance: &'a str,
    pub base_url: &'a str,
}

/// How a request came out.
pub enum Answer {
    /// 2xx: the stream is coming.
    Streaming(reqwest::Response),
    /// Any other status, after the retries a busy one gets.
    Refused(reqwest::Response),
    Interrupted,
}

/// Sends a request, retrying a busy answer or a connection that failed
/// twice before anything streamed, honouring `retry-after`.
pub async fn send(build: &(dyn Fn() -> reqwest::RequestBuilder + Sync), cancel: &watch::Receiver<bool>, peer: Peer<'_>) -> Result<Answer, EngineError> {
    for attempt in 1..=ATTEMPTS {
        let mut cancel_wait = cancel.clone();
        let r = tokio::select! {
            r = build().send() => r,
            _ = crate::engine::cancelled(&mut cancel_wait) => return Ok(Answer::Interrupted),
        };
        let wait = match r {
            Ok(r) if r.status().is_success() => return Ok(Answer::Streaming(r)),
            Ok(r) if attempt < ATTEMPTS && retryable(r.status().as_u16()) => retry_delay(&r, attempt),
            Ok(r) => return Ok(Answer::Refused(r)),
            Err(e) if attempt < ATTEMPTS && (e.is_connect() || e.is_timeout()) => Duration::from_secs(u64::from(attempt)),
            Err(e) => return Err(transport_error(&e, peer)),
        };
        let mut cancel_wait = cancel.clone();
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = crate::engine::cancelled(&mut cancel_wait) => return Ok(Answer::Interrupted),
        }
    }
    unreachable!("the last attempt returns")
}

pub fn retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// `retry-after` when the API gives one (capped), else a short backoff.
fn retry_delay(r: &reqwest::Response, attempt: u32) -> Duration {
    let named = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    Duration::from_secs(named.unwrap_or(u64::from(attempt) * 2).min(30))
}

/// An error answer's status and what its body says, in whichever of the
/// shapes providers use: `{"error": {"type"|"code", "message"}}`,
/// `{"error": "…"}`, or `{"message": "…"}` — else the body's start.
pub async fn refusal(r: reqwest::Response) -> (u16, String) {
    let status = r.status().as_u16();
    // An error body is a sentence, not a stream: one that does not arrive
    // promptly is not worth holding the turn (or an interrupt) for.
    let body = tokio::time::timeout(Duration::from_secs(10), r.text()).await.ok().and_then(Result::ok).unwrap_or_default();
    let said = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let (kind, message) = match v.get("error") {
            Some(Value::String(m)) => (None, Some(m.clone())),
            Some(e) => (s(e, "type").or_else(|| s(e, "code")), s(e, "message")),
            None => (s(&v, "code"), s(&v, "message")),
        };
        match (kind, message) {
            (Some(k), Some(m)) if !m.is_empty() => Some(format!("{k}: {m}")),
            (_, Some(m)) if !m.is_empty() => Some(m),
            (Some(k), _) => Some(k),
            _ => None,
        }
    });
    (status, said.unwrap_or_else(|| if body.trim().is_empty() { format!("HTTP {status}") } else { clip(body.trim(), 300) }))
}

/// A refusal read and worded: `refusal` and `status_error` together, with
/// when a rate limit lifts when the answer's headers say (R-INST-7).
pub async fn refused(r: reqwest::Response, peer: Peer<'_>, model: &str, auth_fix: &str) -> EngineError {
    let resets = resets_at(r.headers(), krowk_store::now_ms());
    let (status, said) = refusal(r).await;
    let e = status_error(status, &said, peer, model, auth_fix);
    if e.limited() { e.with_resets(resets) } else { e }
}

/// When a limit lifts, in milliseconds since the epoch, as a 429's headers
/// say: `retry-after` (seconds, or an HTTP date is not read), Anthropic's
/// `anthropic-ratelimit-*-reset` (RFC 3339), OpenAI's and xAI's
/// `x-ratelimit-reset-*` (`1s`, `6m0s`, `20ms`) — the latest of them.
pub fn resets_at(h: &reqwest::header::HeaderMap, now_ms: i64) -> Option<i64> {
    let mut at: Option<i64> = None;
    // A time in the past, or further off than any provider's window, is
    // a header krowk misread or a server's mistake: not when a limit lifts.
    let mut later = |t: i64| {
        if (now_ms..=now_ms.saturating_add(MAX_RESET_MS)).contains(&t) {
            at = Some(at.map_or(t, |a| a.max(t)));
        }
    };
    for (name, value) in h {
        let (name, Ok(v)) = (name.as_str(), value.to_str()) else { continue };
        let v = v.trim();
        if name == "retry-after" {
            if let Ok(s) = v.parse::<i64>()
                && s >= 0
            {
                later(now_ms.saturating_add(s.saturating_mul(1000)));
            }
        } else if name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset") {
            if let Some(t) = rfc3339_ms(v) {
                later(t);
            }
        } else if name.starts_with("x-ratelimit-reset-")
            && let Some(d) = duration_ms(v)
        {
            later(now_ms.saturating_add(d));
        }
    }
    at
}

/// How much of its rate limit an instance has used, as a successful
/// answer's headers say (R-INST-6): the most used of the request and token
/// windows Anthropic (`anthropic-ratelimit-<what>-limit` / `-remaining`) or
/// OpenAI and xAI (`x-ratelimit-limit-<what>` / `x-ratelimit-remaining-<what>`)
/// report, a warning from 80%. None when they report none.
pub fn limits_of(h: &reqwest::header::HeaderMap, now_ms: i64) -> Option<LimitStatus> {
    let num = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
    let mut best: Option<(f64, String)> = None;
    for what in ["requests", "tokens", "input-tokens", "output-tokens"] {
        let pairs = [(format!("anthropic-ratelimit-{what}-limit"), format!("anthropic-ratelimit-{what}-remaining")), (format!("x-ratelimit-limit-{what}"), format!("x-ratelimit-remaining-{what}"))];
        for (limit, remaining) in pairs {
            if let (Some(l), Some(r)) = (num(&limit), num(&remaining))
                && l > 0.0
            {
                let used = ((l - r) / l * 100.0).clamp(0.0, 100.0);
                if best.as_ref().is_none_or(|(b, _)| used > *b) {
                    best = Some((used, what.to_string()));
                }
            }
        }
    }
    let (used, window) = best?;
    let status = if used >= 100.0 { LimitState::Limited } else if used >= 80.0 { LimitState::Warning } else { LimitState::Allowed };
    Some(LimitStatus { status, window: Some(window), used_percent: Some(used), resets_at_ms: resets_at(h, now_ms) })
}

/// The furthest off a limit's reset is believed: thirty days, past every
/// provider's longest window.
const MAX_RESET_MS: i64 = 30 * 24 * 3600 * 1000;

/// An RFC 3339 UTC time (`2026-09-26T14:00:00Z`, fractions and a numeric
/// offset allowed) in milliseconds since the epoch.
fn rfc3339_ms(s: &str) -> Option<i64> {
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

/// A duration as OpenAI writes one: `20ms`, `1s`, `6m0s`, `1h2m3.5s`.
fn duration_ms(s: &str) -> Option<i64> {
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        let unit = match c {
            'h' => 3_600_000.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                1.0
            }
            'm' => 60_000.0,
            's' => 1_000.0,
            _ => return None,
        };
        total += v * unit;
        any = true;
    }
    (any && num.is_empty() && (0.0..1e15).contains(&total)).then_some(total as i64)
}

/// A refusal as the engine's error, worded for the provider. `auth_fix`
/// says what to do about a credential it refused.
pub fn status_error(status: u16, said: &str, peer: Peer<'_>, model: &str, auth_fix: &str) -> EngineError {
    let v = peer.vendor;
    let e = match status {
        401 => EngineError::new("provider_auth", format!("{v} refused the credentials of the {} instance ({said}) — {auth_fix}", peer.instance)),
        403 => EngineError::new("provider_forbidden", format!("{v} refused the request ({said}) — the {} instance may not have access to {model}", peer.instance)),
        404 => EngineError::new("model_not_found", format!("{v} does not know the model {model:?} ({said}) — pass a current model id with --model")),
        429 => EngineError::new("rate_limited", format!("{v} is rate-limiting the {} instance ({said}) — wait and retry, or use another instance", peer.instance)),
        s if s >= 500 => EngineError::new("provider_unavailable", format!("{v} answered HTTP {s} ({said}) — retry shortly")),
        _ => EngineError::new("provider_invalid_request", format!("{v} refused the request ({said})")),
    };
    e.with_status(status)
}

pub fn transport_error(e: &reqwest::Error, peer: Peer<'_>) -> EngineError {
    // reqwest's own text names a URL and a cause chain; the cause is the news.
    let mut cause = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        cause = s.to_string();
        src = s.source();
    }
    // R-OFF-1: the sentence a person reads first says what is wrong in the
    // words the TUI's notice uses, never a generic failure.
    if e.is_timeout() {
        return EngineError::new(
            "network_unreachable",
            format!("no network connectivity: {} stopped answering ({cause}) — check the network, then run the prompt again with --resume", peer.base_url),
        );
    }
    EngineError::new(
        "network_unreachable",
        format!("no network connectivity: {} could not be reached ({cause}) — check the network, or the base URL of the {} instance", peer.base_url, peer.instance),
    )
}

/// One wire API's stream, decoded into items as its events arrive.
pub trait Decode {
    /// Folds one event in; returns what the client should be told.
    fn apply(&mut self, ev: &SseEvent) -> Result<Vec<EngineEvent>, EngineError>;
    /// The response is whole.
    fn done(&self) -> bool;
    /// The connection closed without the end marker. `Some` when what
    /// arrived is a whole response all the same — a server that sends no
    /// `[DONE]` — with the events that finish it; `None` when it was cut.
    fn ended(&mut self) -> Option<Vec<EngineEvent>> {
        None
    }
    /// What survives an interrupt, completed.
    fn interrupt(&mut self) -> Vec<EngineEvent>;
    fn finish(self, requested_model: &str, interrupted: bool) -> ModelResponse;
}

/// Reads a streaming answer into `dec` to its end, forwarding every item
/// event as it happens.
pub async fn read_stream<D: Decode>(mut resp: reqwest::Response, mut dec: D, events: &Events, cancel: &watch::Receiver<bool>, model: &str, peer: Peer<'_>) -> Result<ModelResponse, EngineError> {
    if let Some(l) = limits_of(resp.headers(), krowk_store::now_ms()) {
        let _ = events.send(EngineEvent::Limits(l)).await;
    }
    let mut parser = SseParser::default();
    let mut cancel_wait = cancel.clone();
    loop {
        let chunk = tokio::select! {
            c = resp.chunk() => c.map_err(|e| transport_error(&e, peer))?,
            _ = crate::engine::cancelled(&mut cancel_wait) => {
                for ev in dec.interrupt() {
                    let _ = events.send(ev).await;
                }
                return Ok(dec.finish(model, true));
            }
        };
        let ended = chunk.is_none();
        let evs = match chunk {
            Some(bytes) => parser.push(&bytes),
            None => parser.finish().into_iter().collect(),
        };
        for sse in &evs {
            for ev in dec.apply(sse)? {
                let _ = events.send(ev).await;
            }
            if dec.done() {
                break;
            }
        }
        if dec.done() {
            return Ok(dec.finish(model, false));
        }
        if ended {
            if let Some(last) = dec.ended() {
                for ev in last {
                    let _ = events.send(ev).await;
                }
                return Ok(dec.finish(model, false));
            }
            return Err(EngineError::new(
                "provider_unavailable",
                format!("the {} stream ended before the response did — the connection dropped; run the prompt again with --resume", peer.vendor),
            ));
        }
    }
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn r_inst_6_a_native_apis_rate_limit_headers_are_read_per_call() {
        let now = 1_790_000_000_000;
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("2026-09-26T14:00:00Z"), Some(1_790_431_200_000));
        assert_eq!(rfc3339_ms("2026-09-26T16:00:00.250+02:00"), Some(1_790_431_200_250));
        assert_eq!(rfc3339_ms("yesterday"), None);
        assert_eq!([duration_ms("20ms"), duration_ms("1s"), duration_ms("6m0s"), duration_ms("1h2m3.5s"), duration_ms("soon")], [Some(20), Some(1000), Some(360_000), Some(3_723_500), None]);
        let a = limits_of(&headers(&[("anthropic-ratelimit-requests-limit", "50"), ("anthropic-ratelimit-requests-remaining", "45"), ("anthropic-ratelimit-tokens-limit", "1000"), ("anthropic-ratelimit-tokens-remaining", "100"), ("anthropic-ratelimit-tokens-reset", "2026-09-26T14:00:00Z")]), now).unwrap();
        assert_eq!((a.status, a.window.as_deref(), a.used_percent.map(|u| u.round()), a.resets_at_ms), (LimitState::Warning, Some("tokens"), Some(90.0), Some(1_790_431_200_000)));
        let o = limits_of(&headers(&[("x-ratelimit-limit-requests", "100"), ("x-ratelimit-remaining-requests", "99"), ("x-ratelimit-reset-requests", "6m0s")]), now).unwrap();
        assert_eq!((o.status, o.resets_at_ms), (LimitState::Allowed, Some(now + 360_000)));
        assert!(limits_of(&headers(&[("content-type", "text/event-stream")]), now).is_none());
        assert_eq!(resets_at(&headers(&[("retry-after", "30")]), now), Some(now + 30_000), "R-INST-7: a 429 says when it lifts");
        // Nothing negative, absurd or overflowing is believed.
        for bad in ["-5", "9223372036854775807", "99999999999"] {
            assert_eq!(resets_at(&headers(&[("retry-after", bad)]), now), None, "{bad}");
        }
        assert_eq!(resets_at(&headers(&[("x-ratelimit-reset-tokens", "99999999999h")]), now), None);
        assert_eq!(resets_at(&headers(&[("anthropic-ratelimit-tokens-reset", "1999-01-01T00:00:00Z")]), now), None, "a reset in the past");
        assert_eq!(rfc3339_ms("2026-13-40T99:00:00Z"), None);
    }
}
