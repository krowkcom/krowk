//! The person's hosts, as the phone picks one, and a session started on
//! one (R-HOST-3, R-HOST-4): what `krowk hosts` and `krowk sync new` do,
//! behind [`Account`]. Each host's announcement is opened against the
//! device list this phone pinned, as `list_sessions` checks records; the
//! start is sealed to the person's devices, bound to its id and the two
//! devices, and so is the answer. Every call here is a blocking round trip
//! (or several) to the registry: Swift calls them off the main thread.
//!
//! The start carries the place, the model and the words only. It names no
//! permission mode, toolset, effort or budget: the command has no such
//! field, and the host runs the session in its own default mode.

use std::sync::PoisonError;
use std::time::{Duration, Instant};

use krowk_client::e2e::DeviceId;
use krowk_client::host_command::{self, HostAnswer, HostCommand};
use krowk_client::presence;
use krowk_client::protocol::{LimitStatus, ModelRef};

use crate::KrowkError;
use crate::account::{Account, internal};

/// How many times a start is sent again after a transport failure, the
/// same id with the same bytes: the registry answers a repeat 200, and the
/// host runs an id once.
const SEND_RETRIES: u32 = 3;
/// How often the host's answer is asked for, and for how long: a host sees
/// a command within one 10 s refresh.
const ANSWER_EVERY: Duration = Duration::from_millis(500);
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// How many sealed starts an account keeps for their retries.
const KEPT_STARTS: usize = 16;

/// The starts an account sealed lately: the command id, the host it is to,
/// the command and its sealed bytes. A seal is never the same bytes twice,
/// and the registry takes an id again only with the same bytes, so a retry
/// of a start re-sends what was sealed for it.
#[derive(Default)]
pub(crate) struct Sealed(std::collections::VecDeque<(String, DeviceId, HostCommand, Vec<u8>)>);

impl Sealed {
    /// The bytes sealed for `id`, when it was this command to this host;
    /// `Err` when the id was used for another.
    fn get(&self, id: &str, to: &DeviceId, command: &HostCommand) -> Result<Option<Vec<u8>>, KrowkError> {
        match self.0.iter().find(|(i, ..)| i == id) {
            None => Ok(None),
            Some((_, t, c, sealed)) if t == to && c == command => Ok(Some(sealed.clone())),
            Some(_) => Err(KrowkError::InvalidInput { detail: format!("command id {id} was used for another start — mint a new one with newCommandId()") }),
        }
    }

    fn keep(&mut self, id: String, to: DeviceId, command: HostCommand, sealed: Vec<u8>) {
        if self.0.len() >= KEPT_STARTS {
            self.0.pop_front();
        }
        self.0.push_back((id, to, command, sealed));
    }
}

/// The hosts whose announcements this phone opened, online first and then
/// by name, and how many more it could not open: sealed under a user key
/// it does not hold, or from a device its list does not name.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HostList {
    pub hosts: Vec<HostRecord>,
    pub unreadable: u32,
}

/// One host as it announced itself. The name and os are the device list's,
/// not the host's own claim.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HostRecord {
    /// Its device id, hex: what `start_session` takes.
    pub device: String,
    pub name: String,
    pub os: String,
    /// Its announcement is unexpired: it refreshed within its TTL.
    pub online: bool,
    /// When it last refreshed its announcement, in milliseconds since the
    /// epoch; 0 when the registry's time does not read.
    pub last_seen_ms: u64,
    /// krowk's version on it: "0.31.0", or "dev".
    pub version: String,
    pub instances: Vec<InstanceRecord>,
    /// Absolute paths it will start a session in.
    pub directories: Vec<String>,
    /// The ids of the sessions its daemon holds.
    pub sessions: Vec<String>,
}

/// One of a host's instances, and whether a session can start on it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct InstanceRecord {
    /// The instance's name: "anthropic".
    pub name: String,
    pub kind: String,
    pub ready: bool,
    /// Its readiness state: "ready", "keyNotSet".
    pub state: String,
    /// The fix line, when it is not ready.
    pub fix: Option<String>,
    /// Model ids, newest first; empty when not ready.
    pub models: Vec<String>,
    /// The last limits frame the host saw for it.
    pub limit: Option<LimitStatus>,
}

/// How a start came out.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum StartAnswer {
    /// The session started on the host; its first turn runs there.
    Started { session_id: String },
    /// Nothing started: `message` is the host's fix line, or, as
    /// `no_answer`, that it didn't answer in time.
    Refused { code: String, message: String },
}

impl From<presence::AnnouncedInstance> for InstanceRecord {
    fn from(i: presence::AnnouncedInstance) -> InstanceRecord {
        InstanceRecord { name: i.name, kind: i.kind, ready: i.ready, state: i.state, fix: i.fix, models: i.models, limit: i.limit }
    }
}

#[uniffi::export]
impl Account {
    /// The person's hosts, from their announcements (R-HOST-3): each opened
    /// against the device list this phone holds, online first, then by
    /// name. Those that do not open are counted in `unreadable`, not an
    /// error. This phone is never listed: a phone never hosts (R-HAND-4).
    /// `not_paired` until it holds the user keys and the list. Blocking:
    /// one registry call.
    pub fn list_hosts(&self) -> Result<HostList, KrowkError> {
        let (keys, chain) = self.paired()?;
        // A registry that keeps no presences yet has none to list.
        let presences = match self.client.list_presences() {
            Ok(p) => p,
            Err(e) if e.status == 404 => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let me = self.device.to_string();
        let mut unreadable = 0u32;
        let mut hosts: Vec<HostRecord> = presences
            .into_iter()
            .filter(|p| !p.device.eq_ignore_ascii_case(&me))
            .filter_map(|p| {
                let opened = DeviceId::parse(&p.device).ok_or_else(|| "not a device id".to_string()).and_then(|d| presence::open(&p.sealed, &d, &keys, &chain));
                let Ok(a) = opened else {
                    unreadable = unreadable.saturating_add(1);
                    return None;
                };
                let last_seen_ms = p.updated_at.parse::<jiff::Timestamp>().ok().and_then(|t| u64::try_from(t.as_millisecond()).ok()).unwrap_or(0);
                Some(HostRecord {
                    device: a.device,
                    name: a.name,
                    os: a.os,
                    online: p.online,
                    last_seen_ms,
                    version: a.version,
                    instances: a.instances.into_iter().map(Into::into).collect(),
                    directories: a.directories,
                    sessions: a.sessions,
                })
            })
            .collect();
        hosts.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.device.cmp(&b.device)));
        Ok(HostList { hosts, unreadable })
    }

    /// Asks host `device` to start a session in `cwd`, one of the
    /// directories it announced, its first turn `prompt` on `model`
    /// (R-HOST-4), as `krowk sync new` does: the start sealed to the
    /// person's devices under `id` (`new_command_id`, minted once per tap
    /// and passed again on every retry of that start: this account sends
    /// the bytes it sealed for it again), sent — again with the same id
    /// and bytes after a transport failure, up to three times — and the
    /// host's answer asked for every 500 ms for up to 30 s.
    /// A host that hasn't answered by then is `Refused` as `no_answer`; it
    /// may still start the session. The host runs it in its own default
    /// permission mode: the start names none.
    ///
    /// `InvalidInput` for a device or id that is not one, an id this
    /// account used for another start, or a prompt too long to send;
    /// `Refused` as `no_host` for a device not on the list this phone
    /// holds, or this phone itself, which never hosts; `command_id_taken`
    /// for an id the registry holds other bytes under (another run of the
    /// app's); `Network` when the start could not be sent, which
    /// may still have reached the host. Blocking, for up to 30 s and the
    /// sends: never on the main thread.
    pub fn start_session(&self, device: String, cwd: String, model: ModelRef, prompt: String, id: String) -> Result<StartAnswer, KrowkError> {
        let (keys, chain) = self.paired()?;
        let to = DeviceId::parse(&device).ok_or_else(|| KrowkError::InvalidInput { detail: format!("{device:?} is not a device id") })?;
        if !uuid_shaped(&id) {
            return Err(KrowkError::InvalidInput { detail: format!("{id:?} is not a command id — mint one with newCommandId()") });
        }
        let host = chain
            .devices()
            .iter()
            .find(|d| d.id() == to && d.kind == krowk_client::device_chain::Kind::Device && to != self.device)
            .ok_or_else(|| KrowkError::Refused { code: "no_host".into(), detail: "that host is not one of your computers".into() })?;
        let name = host.name.clone();
        let start = HostCommand::Start { cwd, model, prompt };
        let kept = self.starts.lock().unwrap_or_else(PoisonError::into_inner).get(&id, &to, &start)?;
        let sealed = match kept {
            Some(sealed) => sealed,
            None => {
                let sealed = host_command::seal_command(&start, &id, &self.device, &to, &keys, chain.generation()).map_err(|detail| KrowkError::InvalidInput { detail })?;
                self.starts.lock().unwrap_or_else(PoisonError::into_inner).keep(id.clone(), to, start, sealed.clone());
                sealed
            }
        };
        let to_hex = to.to_string();
        let mut tries = 0;
        loop {
            match self.client.send_host_command(&to_hex, &id, &sealed) {
                Ok(_) => break,
                Err(e) if transient(&e) && tries < SEND_RETRIES => {
                    tries += 1;
                    std::thread::sleep(ANSWER_EVERY * tries);
                }
                // The last try may have landed with its answer lost.
                Err(e) if transient(&e) => {
                    return Err(KrowkError::Network { detail: format!("the start could not be sent to {name} — it may still have reached it and start the session") });
                }
                Err(e) => return Err(e.into()),
            }
        }
        let deadline = Instant::now() + ANSWER_WAIT;
        let answer = loop {
            match self.client.host_command(&to_hex, &id) {
                Ok(s) if s.answered => {
                    let Some(sealed) = s.answer else { return Err(internal(format!("{name} answered with nothing this phone reads"))) };
                    break host_command::open_answer(&sealed, &id, &to, &self.device, &keys, &chain).map_err(internal)?;
                }
                Ok(_) => {}
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
            if Instant::now() >= deadline {
                return Ok(StartAnswer::Refused { code: "no_answer".into(), message: format!("{name} didn't answer within 30 s — it may still start the session") });
            }
            std::thread::sleep(ANSWER_EVERY);
        };
        Ok(match answer {
            HostAnswer::Started { session_id } if uuid_shaped(&session_id) => StartAnswer::Started { session_id },
            HostAnswer::Started { session_id } => return Err(internal(format!("{name} started a session under {:?}, which is not a session id", session_id))),
            HostAnswer::Refused { code, message } => StartAnswer::Refused { code, message },
        })
    }
}

/// A fresh command id for `Account::start_session`: a random UUID, version
/// 4, lower case. Minted once per start the person asks for; every retry
/// of that start passes the same one, so the host runs it once.
#[uniffi::export]
pub fn new_command_id() -> String {
    let mut b = krowk_client::e2e::random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = krowk_client::e2e::hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Whether a failure leaves the call worth making again: nothing answered,
/// a rate limit or a 5xx.
fn transient(e: &krowk_api::Error) -> bool {
    e.status == 0 || e.status == 429 || e.status >= 500
}

/// Whether `s` is a UUID as text: 8-4-4-4-12 hex digits.
fn uuid_shaped(s: &str) -> bool {
    s.len() == 36 && s.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })
}
