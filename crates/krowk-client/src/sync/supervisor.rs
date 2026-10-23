//! When a phone's viewer connects, and when it tries again (R-IOS-2,
//! R-OFF-2, R-LAG-9; ticket 86): the rules a phone needs on top of
//! `ViewerCore`, which backgrounds constantly, has its socket dropped by
//! iOS about 30 s into the background, and changes networks (Wi-Fi,
//! cellular, a VPN) all the time. Without them it shows a dead socket as
//! live, hammers the relay with retries, or waits for a timeout before it
//! notices it is back in the foreground.
//!
//! Pure, like the core: the caller passes the time (`now_ms`, monotonic)
//! and, where a backoff is drawn, a random byte; nothing here reads a
//! clock or a random source, so the rules run under `cargo test` with a
//! fake clock. krowk-ffi's `Viewer` asks it on every loss, join and
//! message, and the app forwards the scene's foreground and background,
//! path changes and pull-to-refresh to it.
//!
//! - **Backoff.** From 250 ms, doubling, capped at 30 s, each wait cut by
//!   up to half at random (`full - full / 2 * random / 255`, so it lies in
//!   `[full / 2, full]`). It starts over only once a connection has stayed
//!   up 30 s: a link that flaps keeps backing off.
//! - **Wake** (the scene becoming active, a network path that became
//!   usable, pull-to-refresh): with no connection, join now, no backoff;
//!   with one silent past `DEAD`, replace it now; else send a `ping` and
//!   replace it if nothing comes within 5 s.
//! - **Background.** A live connection is kept, but a lost one is not
//!   tried again until the foreground: push covers a finished turn and an
//!   approval meanwhile.
//! - **Foreground.** After 10 s or more away, the connection is replaced at
//!   once instead of probed: iOS has likely dropped the socket, and a probe
//!   would hold a dead one until it timed out. A shorter absence is a wake.

use super::DEAD;
use std::time::Duration;

/// The first wait before trying again.
pub const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// The longest wait before trying again.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How long a connection stays up before the backoff starts over.
pub const STAYED_UP: Duration = Duration::from_secs(30);
/// How long a wake's `ping` has for something to come back.
pub const PROBE: Duration = Duration::from_secs(5);
/// Away this long, the connection is replaced on the foreground rather
/// than probed.
pub const LONG_BACKGROUND: Duration = Duration::from_secs(10);

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Where the viewer's connection stands, as its caller knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// None, and none being made: waiting to try again, or held in the
    /// background.
    Down,
    /// One being made: its ticket fetched or its join under way.
    Joining,
    /// Joined.
    Up,
}

/// What a wake or a foreground asks of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Nothing: a connection is being made, a probe is already out, or
    /// the app is in the background.
    Nothing,
    /// No connection: join now, with no backoff.
    Connect,
    /// Drop the connection and join again now: it has been silent past
    /// `DEAD`, or the app was away `LONG_BACKGROUND` or more.
    Replace,
    /// Send the text `ping`; `probe_due` says when nothing came back in
    /// time and the connection is to be replaced.
    Probe,
}

/// The wait before the next try, `full` cut by up to half: `random` 0
/// waits `full`, 255 waits `full / 2`.
pub fn jittered(full_ms: u64, random: u8) -> u64 {
    full_ms - (full_ms / 2) * u64::from(random) / 255
}

/// One viewer's connection lifecycle. See the module's rules.
#[derive(Debug, Clone)]
pub struct ConnectionSupervisor {
    /// The next wait before it is cut at random.
    full_ms: u64,
    /// When the current connection joined.
    up_since: Option<u64>,
    /// When the current connection last brought anything.
    heard_ms: u64,
    /// The probe out, and by when something must come back.
    probe_until: Option<u64>,
    /// In the background since.
    background_since: Option<u64>,
}

impl ConnectionSupervisor {
    pub fn new(now_ms: u64) -> ConnectionSupervisor {
        ConnectionSupervisor { full_ms: ms(BACKOFF_BASE), up_since: None, heard_ms: now_ms, probe_until: None, background_since: None }
    }

    /// The connection ends (lost or replaced): the backoff starts over if
    /// it stayed up `STAYED_UP`, and nothing of it is waited on any more.
    fn ended(&mut self, now_ms: u64) {
        if self.up_since.is_some_and(|t| now_ms.saturating_sub(t) >= ms(STAYED_UP)) {
            self.full_ms = ms(BACKOFF_BASE);
        }
        self.up_since = None;
        self.probe_until = None;
    }

    /// The connection was lost, or a join (its ticket, dial or handshake)
    /// failed: when to try again, `random` drawing the cut; `None` in the
    /// background, where it is held until the foreground.
    pub fn lost(&mut self, now_ms: u64, random: u8) -> Option<u64> {
        self.ended(now_ms);
        if self.background_since.is_some() {
            return None;
        }
        let wait = jittered(self.full_ms, random);
        self.full_ms = self.full_ms.saturating_mul(2).min(ms(BACKOFF_CAP));
        Some(now_ms.saturating_add(wait))
    }

    /// The connection is dropped and joined again at once (`Replace`, or
    /// the core's own dead-link rejoin): no loss to back off from, but a
    /// connection that stayed up `STAYED_UP` starts the backoff over, as a
    /// loss would.
    pub fn replaced(&mut self, now_ms: u64) {
        self.ended(now_ms);
    }

    /// A connection joined.
    pub fn joined(&mut self, now_ms: u64) {
        self.up_since = Some(now_ms);
        self.heard_ms = now_ms;
        self.probe_until = None;
    }

    /// Anything came on the current connection, a `pong` included: it
    /// lives, and a probe out is answered.
    pub fn heard(&mut self, now_ms: u64) {
        self.heard_ms = now_ms;
        self.probe_until = None;
    }

    /// A wake: the scene became active, a network path became usable, or
    /// the person pulled to refresh. Nothing in the background.
    pub fn wake(&mut self, now_ms: u64, link: Link) -> Wake {
        if self.background_since.is_some() {
            return Wake::Nothing;
        }
        match link {
            Link::Down => Wake::Connect,
            Link::Joining => Wake::Nothing,
            Link::Up if now_ms.saturating_sub(self.heard_ms) > ms(DEAD) => {
                self.replaced(now_ms);
                Wake::Replace
            }
            Link::Up if self.probe_until.is_some() => Wake::Nothing,
            Link::Up => {
                self.probe_until = Some(now_ms.saturating_add(ms(PROBE)));
                Wake::Probe
            }
        }
    }

    /// The scene went to the background: a live connection is kept, a lost
    /// one is held.
    pub fn background(&mut self, now_ms: u64) {
        self.background_since.get_or_insert(now_ms);
        self.probe_until = None;
    }

    /// The scene is coming to the foreground: away `LONG_BACKGROUND` or
    /// more, a connection still up is replaced; otherwise a wake.
    pub fn foreground(&mut self, now_ms: u64, link: Link) -> Wake {
        let away = self.background_since.take().map(|t| now_ms.saturating_sub(t));
        if link == Link::Up && away.is_some_and(|a| a >= ms(LONG_BACKGROUND)) {
            self.replaced(now_ms);
            return Wake::Replace;
        }
        self.wake(now_ms, link)
    }

    /// Whether the probe out has gone unanswered past `PROBE`.
    pub fn probe_due(&self, now_ms: u64) -> bool {
        self.probe_until.is_some_and(|t| now_ms >= t)
    }

    /// `probe_due`, and the probe done with either way once it is: a
    /// deadline that has passed is never asked about again, so a caller
    /// that can't act on it (the connection gone, or being replaced) is
    /// never woken for it again and again.
    pub fn take_probe_due(&mut self, now_ms: u64) -> bool {
        let due = self.probe_due(now_ms);
        if due {
            self.probe_until = None;
        }
        due
    }

    /// When `probe_due` next turns true, if a probe is out.
    pub fn next_wake(&self) -> Option<u64> {
        self.probe_until
    }

    /// In the background.
    pub fn in_background(&self) -> bool {
        self.background_since.is_some()
    }
}
