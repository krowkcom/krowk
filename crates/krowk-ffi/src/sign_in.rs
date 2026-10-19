//! Signing the phone in through the browser, as `krowk login` does: the
//! phone opens a login and polls for its key; the browser only approves
//! (Canon, architecture.md → "A browser login splits collecting the key
//! from approving it"). The polling rules are `krowk_api::login`'s, the
//! same ones the terminal keeps (R-CLIENT-1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use krowk_api::login::{LoginPoll, Next, browsable_url};
use krowk_api::{CliAuthorization, Client, LoginAction};
use krowk_client::Zeroizing;
use serde::{Deserialize, Serialize};

use crate::KrowkError;
use crate::storage::{CREDENTIALS, KeyStorage};

/// The longest a sleep goes without looking at the cancel flag.
const SLICE: Duration = Duration::from_millis(250);

/// A browser sign-in under way: its code and page to show, and `wait` to
/// collect the key once somebody approves it.
#[derive(uniffi::Object)]
pub struct SignIn {
    client: Client,
    /// Holds the slug, which collects the key: never handed to Swift.
    auth: CliAuthorization,
    /// The approval page, checked to be http(s).
    page: String,
    cancelled: AtomicBool,
}

/// Who the phone signed in as.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SignedIn {
    pub workspace: String,
    pub workspace_name: String,
    pub key_id: String,
}

/// The `credentials.json` storage item: what `krowk login` keeps in
/// credentials.json for one workspace, as one blob. The token is wiped
/// when this is dropped.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Credentials {
    #[serde(default)]
    pub version: u8,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub key_id: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub workspace_name: String,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.token.zeroize();
    }
}

#[uniffi::export]
impl SignIn {
    /// Opens a browser sign-in against the registry at `api` (its `/v1`
    /// URL), named after the phone — the approval page asks about
    /// `device_name`, and the key is named after it. Its control
    /// characters are dropped and it is cut to 64 bytes.
    #[uniffi::constructor]
    pub fn start(api: String, device_name: String) -> Result<Arc<SignIn>, KrowkError> {
        // Keyless: the endpoint exists for a device with no key.
        let client = Client::new(&api, "");
        let auth = client.start_cli_authorization(LoginAction::Login, &device_name)?;
        let page = browsable_url(&auth.verification_url, client.insecure())
            .map_err(|e| KrowkError::Refused { code: "unsafe_url".into(), detail: e.fix() })?;
        Ok(Arc::new(SignIn { client, auth, page, cancelled: AtomicBool::new(false) }))
    }

    /// The code the person checks on the approval page, like `K7QF-9M3X`.
    pub fn code(&self) -> String {
        self.auth.code.clone()
    }

    /// The approval page, to open in the browser: https, or http only when
    /// the API itself is.
    pub fn verification_url(&self) -> String {
        self.page.clone()
    }

    /// When the sign-in lapses unapproved, as the registry wrote it (RFC 3339).
    pub fn expires_at(&self) -> String {
        self.auth.expires_at.clone()
    }

    /// Blocks until the sign-in is approved, denied, lapses or is
    /// cancelled; call it off the main thread. On approval the key is saved
    /// to `storage`'s `credentials.json` item before this returns. Refused
    /// with `authorization_denied`, `authorization_expired` or `cancelled`
    /// — or `spent` when this sign-in's key was collected already, by an
    /// earlier `wait`; `Network` when the registry could not be reached for
    /// the whole window.
    pub fn wait(&self, storage: Arc<dyn KeyStorage>) -> Result<SignedIn, KrowkError> {
        let granted = self.poll()?;
        let creds = Credentials {
            version: 1,
            token: granted.token.clone(),
            key_id: granted.key_id.clone(),
            workspace: granted.workspace.clone(),
            workspace_name: granted.workspace_name.clone(),
        };
        drop(granted);
        save(&*storage, &creds)?;
        Ok(SignedIn { workspace: creds.workspace.clone(), workspace_name: creds.workspace_name.clone(), key_id: creds.key_id.clone() })
    }

    /// Ends a `wait` under way, and any later one at once: a cancelled
    /// sign-in stays cancelled. Within a quarter of a second while `wait`
    /// sleeps between polls, which is nearly all of it; a poll already sent
    /// is answered, or times out, first.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl SignIn {
    fn poll(&self) -> Result<Wiped, KrowkError> {
        let mut poll = LoginPoll::new(&self.auth, Instant::now(), jiff::Timestamp::now());
        loop {
            self.check()?;
            let answer = match poll.next(Instant::now()) {
                Next::Sleep(d) => {
                    self.sleep(d)?;
                    continue;
                }
                Next::Read => match poll.answer(self.client.read_cli_authorization(poll.slug())) {
                    Some(answer) => answer,
                    None => continue,
                },
                Next::Done(answer) => answer,
            };
            return answer.map(Wiped).map_err(refusal);
        }
    }

    fn check(&self) -> Result<(), KrowkError> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(KrowkError::Refused { code: "cancelled".into(), detail: "this sign-in was cancelled".into() });
        }
        Ok(())
    }

    /// Sleeps `d` in slices, checking the cancel flag after each.
    fn sleep(&self, d: Duration) -> Result<(), KrowkError> {
        let until = Instant::now() + d;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(SLICE));
            self.check()?;
        }
    }
}

/// An approved authorization, its token wiped when dropped.
struct Wiped(CliAuthorization);

impl std::ops::Deref for Wiped {
    type Target = CliAuthorization;
    fn deref(&self) -> &CliAuthorization {
        &self.0
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.token.zeroize();
    }
}

/// How the poll ended without a key, in words for the phone rather than the
/// terminal's `krowk login`. The registry's own `expired` is the same
/// lapse seen from its clock, when the phone's runs behind it.
fn refusal(e: krowk_api::Error) -> KrowkError {
    let (code, detail) = match (e.code().as_str(), e.status) {
        ("authorization_denied", _) => ("authorization_denied", "this sign-in was denied in the browser — sign in again to ask again"),
        ("authorization_expired" | "expired", _) => ("authorization_expired", "nobody approved this sign-in before it lapsed — sign in again to ask again"),
        ("spent", _) => ("spent", "this sign-in's key was already collected, and the registry keeps no second copy — sign in again"),
        (_, 404) => ("not_found", "the registry does not know this sign-in — it may have lapsed; sign in again"),
        _ => return e.into(),
    };
    KrowkError::Refused { code: code.into(), detail: detail.into() }
}

/// Writes `creds` to the `credentials.json` item, serialized into a buffer
/// wiped after; the copy UniFFI makes to hand Swift is its own, unwiped
/// (`storage::Foreign` says more). The key was handed over once and the registry keeps no copy, so a failed
/// write says to sign in again.
fn save(storage: &dyn KeyStorage, creds: &Credentials) -> Result<(), KrowkError> {
    let mut buf = Zeroizing::new(Vec::with_capacity(512));
    serde_json::to_writer(&mut *buf, creds).expect("credentials serialize");
    storage.write(CREDENTIALS.into(), buf.to_vec()).map_err(|e| KrowkError::Internal {
        detail: format!("could not save the key to the Keychain item {CREDENTIALS}: {e} — it was handed over once and the registry keeps no copy, so sign in again"),
    })
}
