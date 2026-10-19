//! A browser login's poll, as a state machine with no IO and no clock of its
//! own: the caller sleeps, reads and passes the time in. The CLI drives it
//! with a blocking client and `std::thread::sleep`; a phone drives the same
//! rules with whatever it sleeps and reads with (R-CLIENT-1).

use crate::client::retry_after_for;
use crate::{fail, CliAuthorization, Error, AUTHORIZATION_APPROVED, AUTHORIZATION_DENIED};
use serde_json::json;
use std::time::{Duration, Instant};

/// A browser login's window and pace are the registry's to set, and bounded
/// here, so an absurd answer can neither make krowk hammer it nor leave a
/// forgotten terminal polling all afternoon.
const DEFAULT_WINDOW: Duration = Duration::from_secs(15 * 60);
const MIN_WINDOW: Duration = Duration::from_secs(60);
const MAX_WINDOW: Duration = Duration::from_secs(30 * 60);
const DEFAULT_POLL: Duration = Duration::from_secs(5);
const MIN_POLL: Duration = Duration::from_secs(1);
const MAX_POLL: Duration = Duration::from_secs(30);

/// Polls until somebody answers or the window closes. Approved, denied and a
/// refusal about this authorization are answers; a rate limit, a 5xx or no
/// network are the moment, and the window is kept rather than an approval
/// thrown away.
#[derive(Debug)]
pub struct LoginPoll {
    slug: String,
    interval: Duration,
    deadline: Instant,
    /// The wait owed before the next read, not yet started.
    wait: Option<Duration>,
    /// When the wait under way ends.
    due: Instant,
    /// The last poll that got no answer: if the window closes with one
    /// outstanding, krowk could not ask, and "nobody approved it" would blame
    /// a person for a question that never got out.
    unanswered: Option<Error>,
}

/// What the caller does next.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// Sleep this long, then ask again.
    Sleep(Duration),
    /// Read the authorization back and hand the result to [`LoginPoll::answer`].
    Read,
    /// Stop: the key, or why there is none.
    Done(Result<CliAuthorization, Error>),
}

impl LoginPoll {
    /// The poll for an authorization just opened, at `now` on the monotonic
    /// clock and `wall` on the wall clock its `expires_at` is read against.
    pub fn new(auth: &CliAuthorization, now: Instant, wall: jiff::Timestamp) -> LoginPoll {
        let interval = poll_interval(auth.interval);
        LoginPoll {
            slug: auth.slug.clone(),
            interval,
            deadline: now + window(&auth.expires_at, wall),
            // Slept before the first read too: an authorization milliseconds
            // old cannot have been approved yet.
            wait: Some(interval),
            due: now,
            unanswered: None,
        }
    }

    /// The slug every read is for.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// What to do now: sleep (never past the deadline), read, or stop.
    pub fn next(&mut self, now: Instant) -> Next {
        if now >= self.deadline {
            return Next::Done(Err(window_closed(self.unanswered.take())));
        }
        if let Some(wait) = self.wait.take() {
            self.due = now + wait;
        }
        if now < self.due {
            return Next::Sleep(self.due.min(self.deadline) - now);
        }
        Next::Read
    }

    /// The read's result; returns the outcome once it is an answer.
    pub fn answer(&mut self, read: Result<CliAuthorization, Error>) -> Option<Result<CliAuthorization, Error>> {
        self.wait = Some(self.interval);
        match read {
            Err(e) if worth_another_poll(&e) => {
                if let Some(asked) = retry_after_for(&e).filter(|a| *a > self.interval) {
                    self.wait = Some(asked);
                }
                self.unanswered = Some(e);
                None
            }
            Err(e) => Some(Err(login_fix(e))),
            Ok(granted) if granted.state == AUTHORIZATION_APPROVED => {
                if granted.token.is_empty() {
                    return Some(Err(fail(
                        "malformed_response",
                        "the registry approved this login without handing over a key — run `krowk login` again, and report it if it repeats",
                    )));
                }
                Some(Ok(granted))
            }
            Ok(granted) if granted.state == AUTHORIZATION_DENIED => {
                Some(Err(fail("authorization_denied", "this login was denied in the browser — run `krowk login` to ask again")))
            }
            // Pending, or a state this build has no word for: still inside the window.
            Ok(_) => {
                self.unanswered = None;
                None
            }
        }
    }
}

fn window_closed(unanswered: Option<Error>) -> Error {
    unanswered.unwrap_or_else(|| {
        fail("authorization_expired", "nobody approved this login before it lapsed — run `krowk login` to ask again")
    })
}

fn worth_another_poll(e: &Error) -> bool {
    e.code() == "network_unreachable" || e.status == 429 || e.status >= 500
}

/// Advice written for artifacts, replaced on a login: "upload it again" is
/// nonsense to someone trying to log in.
fn login_fix(mut e: Error) -> Error {
    let fix = match (e.code().as_str(), e.status) {
        ("expired", _) => "this login lapsed before it was approved — run `krowk login` to ask again",
        ("spent", _) => "this login's key was already collected, and the registry keeps no second copy — run `krowk login` for a new one",
        (_, 404) => "the registry does not know this login — it may have lapsed and been swept; run `krowk login` to ask again",
        _ => return e,
    };
    e.body.insert("fix".into(), json!(fix));
    e
}

/// The registry's expiry, clamped: a clock minutes off the registry's would
/// otherwise abandon a good login before its first poll.
fn window(expires_at: &str, wall: jiff::Timestamp) -> Duration {
    match expires_at.parse::<jiff::Timestamp>() {
        Ok(at) => {
            let secs = at.duration_since(wall).as_secs().max(0) as u64;
            Duration::from_secs(secs).clamp(MIN_WINDOW, MAX_WINDOW)
        }
        Err(_) => DEFAULT_WINDOW,
    }
}

fn poll_interval(seconds: i64) -> Duration {
    if seconds <= 0 {
        return DEFAULT_POLL;
    }
    Duration::from_secs(seconds.min(MAX_POLL.as_secs() as i64) as u64).max(MIN_POLL)
}

/// The approval page arrives in a response body and is about to go to the
/// desktop's URL handler, which reaches far past an HTTP client — so only
/// http(s), and plain http only when the API itself is (`api_insecure`,
/// [`crate::Client::insecure`]).
pub fn browsable_url(raw: &str, api_insecure: bool) -> Result<String, Error> {
    let malformed = |what: &str| {
        fail(
            "malformed_response",
            format!("the registry {what} — check KROWK_API_URL points at the API host, not the website"),
        )
    };
    if raw.is_empty() {
        return Err(malformed("opened a browser login without naming a page to approve it on"));
    }
    let Some((scheme, rest)) = raw.split_once("://").or_else(|| raw.split_once(':')) else {
        return Err(malformed("named a page for this login with no scheme"));
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => {}
        "http" if api_insecure => {}
        "http" => {
            return Err(fail(
                "refused_verification_url",
                "the registry named an http page for this login while the API itself is https — krowk will not open it; check KROWK_API_URL",
            ))
        }
        "" => return Err(malformed("named a page for this login with no scheme")),
        other => {
            return Err(fail(
                "refused_verification_url",
                format!("the registry named a {other}: page for this login, and krowk only opens http and https"),
            ))
        }
    }
    if rest.split(['/', '?', '#']).next().unwrap_or_default().rsplit('@').next().unwrap_or_default().is_empty() {
        return Err(malformed("named a page for this login with no host"));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, AUTHORIZATION_PENDING};

    const SEC: Duration = Duration::from_secs(1);

    fn wall() -> jiff::Timestamp {
        "2026-10-19T12:00:00Z".parse().unwrap()
    }

    /// An authorization as the registry opens one: a five-second pace and a
    /// fifteen-minute window from `wall()`.
    fn opened() -> CliAuthorization {
        CliAuthorization {
            slug: "aut_x".into(),
            state: AUTHORIZATION_PENDING.into(),
            interval: 5,
            expires_at: "2026-10-19T12:15:00Z".into(),
            ..CliAuthorization::default()
        }
    }

    fn state(s: &str) -> CliAuthorization {
        CliAuthorization { state: s.into(), ..opened() }
    }

    fn refusal(status: u16, code: &str, retry_after: Option<&str>) -> Error {
        let mut e = Error { status, body: Default::default() };
        e.body.insert("error".into(), json!(code));
        if let Some(r) = retry_after {
            e.body.insert("retry_after".into(), json!(r));
        }
        e
    }

    /// Sleeps whatever the poll asks for until it asks for a read, and
    /// returns the clock then.
    fn until_read(poll: &mut LoginPoll, mut now: Instant) -> Instant {
        loop {
            match poll.next(now) {
                Next::Sleep(d) => now += d,
                Next::Read => return now,
                Next::Done(r) => panic!("stopped before a read: {r:?}"),
            }
        }
    }

    #[test]
    fn r_client_1_the_first_read_waits_one_interval() {
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&opened(), t0, wall());
        assert_eq!(poll.slug(), "aut_x");
        assert_eq!(poll.next(t0), Next::Sleep(5 * SEC));
        // Woken early, it sleeps the rest rather than reading.
        assert_eq!(poll.next(t0 + 2 * SEC), Next::Sleep(3 * SEC));
        assert_eq!(poll.next(t0 + 5 * SEC), Next::Read);
    }

    #[test]
    fn r_client_1_a_429_with_retry_after_stretches_the_next_wait() {
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&opened(), t0, wall());
        let now = until_read(&mut poll, t0);
        assert_eq!(poll.answer(Err(refusal(429, "rate_limited", Some("20")))), None);
        assert_eq!(poll.next(now), Next::Sleep(20 * SEC));
        // A retry_after shorter than the pace does not quicken it.
        let now = until_read(&mut poll, now);
        assert_eq!(poll.answer(Err(refusal(429, "rate_limited", Some("2")))), None);
        assert_eq!(poll.next(now), Next::Sleep(5 * SEC));
    }

    #[test]
    fn r_client_1_a_503_then_an_approval_returns_the_key() {
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&opened(), t0, wall());
        let now = until_read(&mut poll, t0);
        assert_eq!(poll.answer(Err(refusal(503, "http_503", None))), None);
        until_read(&mut poll, now);
        let granted = CliAuthorization { token: "krowk_sk_x".into(), ..state(AUTHORIZATION_APPROVED) };
        assert_eq!(poll.answer(Ok(granted.clone())), Some(Ok(granted)));
    }

    #[test]
    fn r_client_1_no_network_outstanding_at_the_deadline_is_the_error_returned() {
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&opened(), t0, wall());
        let mut now = t0;
        loop {
            match poll.next(now) {
                Next::Sleep(d) => now += d,
                Next::Read => assert_eq!(poll.answer(Err(refusal(0, "network_unreachable", None))), None),
                Next::Done(r) => {
                    assert_eq!(r.unwrap_err().code(), "network_unreachable");
                    break;
                }
            }
        }
        // The last sleep stopped at the deadline, not past it.
        assert_eq!(now, t0 + 15 * 60 * SEC);
    }

    #[test]
    fn r_client_1_pending_until_the_deadline_gives_authorization_expired() {
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&opened(), t0, wall());
        // An unanswered poll that a later pending answer clears.
        let mut now = until_read(&mut poll, t0);
        assert_eq!(poll.answer(Err(refusal(502, "http_502", None))), None);
        let outcome = loop {
            match poll.next(now) {
                Next::Sleep(d) => now += d,
                Next::Read => assert_eq!(poll.answer(Ok(state(AUTHORIZATION_PENDING))), None),
                Next::Done(r) => break r,
            }
        };
        assert_eq!(outcome.unwrap_err().code(), "authorization_expired");
    }

    #[test]
    fn r_client_1_denied_gives_authorization_denied() {
        let mut poll = LoginPoll::new(&opened(), Instant::now(), wall());
        let r = poll.answer(Ok(state(AUTHORIZATION_DENIED))).unwrap();
        assert_eq!(r.unwrap_err().code(), "authorization_denied");
    }

    #[test]
    fn r_client_1_approved_without_a_token_gives_malformed_response() {
        let mut poll = LoginPoll::new(&opened(), Instant::now(), wall());
        let r = poll.answer(Ok(state(AUTHORIZATION_APPROVED))).unwrap();
        assert_eq!(r.unwrap_err().code(), "malformed_response");
    }

    #[test]
    fn r_client_1_a_410_spent_carries_the_spent_fix_line() {
        let mut poll = LoginPoll::new(&opened(), Instant::now(), wall());
        let e = poll.answer(Err(refusal(410, "spent", None))).unwrap().unwrap_err();
        assert_eq!(e.code(), "spent");
        assert!(e.fix().contains("already collected"), "{}", e.fix());
        let e = poll.answer(Err(refusal(404, "not_found", None))).unwrap().unwrap_err();
        assert!(e.fix().contains("does not know this login"), "{}", e.fix());
    }

    #[test]
    fn r_client_1_the_pace_and_window_are_bounded() {
        assert_eq!(poll_interval(0), DEFAULT_POLL);
        assert_eq!(poll_interval(-3), DEFAULT_POLL);
        assert_eq!(poll_interval(1_000_000), MAX_POLL);
        assert_eq!(poll_interval(7), 7 * SEC);
        assert_eq!(window("not a time", wall()), DEFAULT_WINDOW);
        assert_eq!(window("2000-01-01T00:00:00Z", wall()), MIN_WINDOW);
        assert_eq!(window("2026-10-20T00:00:00Z", wall()), MAX_WINDOW);
        assert_eq!(window("2026-10-19T12:10:00Z", wall()), 10 * 60 * SEC);
        // The poll keeps to them: an absurd pace is read as the ceiling.
        let t0 = Instant::now();
        let mut poll = LoginPoll::new(&CliAuthorization { interval: 1_000_000, ..opened() }, t0, wall());
        assert_eq!(poll.next(t0), Next::Sleep(MAX_POLL));
    }

    #[test]
    fn r_client_1_the_approval_page_is_http_s_only_and_plain_http_only_beside_a_plain_api() {
        let prod = Client::new("https://api.krowk.com/v1", "").insecure();
        let local = Client::new("http://127.0.0.1:8787/v1", "").insecure();
        assert!(browsable_url("https://app.krowk.com/cli/ABCD", prod).is_ok());
        assert_eq!(browsable_url("http://app.krowk.com/x", prod).unwrap_err().code(), "refused_verification_url");
        assert!(browsable_url("http://127.0.0.1:8787/x", local).is_ok());
        assert_eq!(browsable_url("file:///etc/passwd", prod).unwrap_err().code(), "refused_verification_url");
        assert_eq!(browsable_url("https:///nohost", prod).unwrap_err().code(), "malformed_response");
        assert_eq!(browsable_url("", prod).unwrap_err().code(), "malformed_response");
    }
}
