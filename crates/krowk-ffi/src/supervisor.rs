//! The phone's connection lifecycle (ticket 86; R-IOS-2, R-OFF-2,
//! R-LAG-9): krowk-client's pure `ConnectionSupervisor`, with the random
//! source its backoff draws from, behind one mutex. One per followed
//! session: Swift asks it directly while the session is still being opened
//! (a failed open is tried again on its backoff), then hands it to
//! `Viewer::open_supervised`, which asks it on every loss, join and
//! message, and takes the scene's foreground and background, path changes
//! and pull-to-refresh as `Viewer::foreground`, `background` and `wake`.
//!
//! The time is always the caller's `now_ms`, monotonic; the randomness is
//! the caller's `seed`, stretched here (SplitMix64) one byte per backoff.
//! Nothing reads a clock or the system's random source.

use std::sync::Mutex;

use krowk_client::sync::supervisor as rules;

/// Where the connection stands, as the caller knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LinkState {
    /// None, and none being made: waiting to try again, or held in the
    /// background.
    Down,
    /// One being made: the session being opened, its ticket fetched, or
    /// its join under way.
    Joining,
    /// Joined.
    Up,
}

/// What a wake or a foreground asks of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum WakeAction {
    /// Nothing: one is being made, a probe is out, or the app is in the
    /// background.
    Nothing,
    /// None there: connect (open the session) now, with no backoff.
    Connect,
    /// Drop it and join again now.
    Replace,
    /// Send the text `ping`, and replace it if nothing comes within 5 s.
    Probe,
}

impl From<LinkState> for rules::Link {
    fn from(l: LinkState) -> rules::Link {
        match l {
            LinkState::Down => rules::Link::Down,
            LinkState::Joining => rules::Link::Joining,
            LinkState::Up => rules::Link::Up,
        }
    }
}

impl From<rules::Wake> for WakeAction {
    fn from(w: rules::Wake) -> WakeAction {
        match w {
            rules::Wake::Nothing => WakeAction::Nothing,
            rules::Wake::Connect => WakeAction::Connect,
            rules::Wake::Replace => WakeAction::Replace,
            rules::Wake::Probe => WakeAction::Probe,
        }
    }
}

pub(crate) struct Inner {
    pub(crate) rules: rules::ConnectionSupervisor,
    random: u64,
}

impl Inner {
    /// One random byte: SplitMix64's next output's top byte.
    pub(crate) fn random(&mut self) -> u8 {
        self.random = self.random.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.random;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 56) as u8
    }

    /// A loss: when to try again, or `None` in the background.
    pub(crate) fn lost(&mut self, now_ms: u64) -> Option<u64> {
        let r = self.random();
        self.rules.lost(now_ms, r)
    }
}

/// One followed session's connection lifecycle: when it connects and when
/// it tries again. See krowk-client's `sync::supervisor` for the rules.
#[derive(uniffi::Object)]
pub struct ConnectionSupervisor {
    pub(crate) inner: Mutex<Inner>,
}

impl ConnectionSupervisor {
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut i = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut i)
    }
}

#[uniffi::export]
impl ConnectionSupervisor {
    /// A supervisor starting at `now_ms`, its backoff's cuts drawn from
    /// `seed` (any random number: the app passes `UInt64.random`).
    #[uniffi::constructor]
    pub fn new(now_ms: u64, seed: u64) -> std::sync::Arc<ConnectionSupervisor> {
        std::sync::Arc::new(ConnectionSupervisor { inner: Mutex::new(Inner { rules: rules::ConnectionSupervisor::new(now_ms), random: seed }) })
    }

    /// Opening the session (or a join) failed: when to try again, on the
    /// backoff; `None` in the background, where it waits for `foreground`.
    pub fn lost(&self, now_ms: u64) -> Option<u64> {
        self.with(|i| i.lost(now_ms))
    }

    /// The scene went to the background.
    pub fn background(&self, now_ms: u64) {
        self.with(|i| i.rules.background(now_ms));
    }

    /// The scene is coming forward, the connection standing at `link`.
    pub fn foreground(&self, now_ms: u64, link: LinkState) -> WakeAction {
        self.with(|i| i.rules.foreground(now_ms, link.into()).into())
    }

    /// The scene became active, a network path became usable, or the
    /// person pulled to refresh, the connection standing at `link`.
    pub fn wake(&self, now_ms: u64, link: LinkState) -> WakeAction {
        self.with(|i| i.rules.wake(now_ms, link.into()).into())
    }

    /// Whether the app is in the background.
    pub fn in_background(&self) -> bool {
        self.with(|i| i.rules.in_background())
    }
}
