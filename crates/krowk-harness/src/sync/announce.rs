//! The host daemon announces itself to the person's other devices (R-HOST-3;
//! engineering/harness.md → Sync → Hosts announce themselves): that it is
//! online, its krowk version, its instances and their models, the
//! directories it starts sessions in and the sessions it holds — sealed to
//! the person's devices (`krowk_client::presence`), so the registry holds
//! only bytes.
//!
//! **The publisher** runs on a thread of its own, `krowk-presence`, beside
//! the supervisor's list reader and never on the supervisor's runtime, which
//! so gains no timer (R-PERF-2). It is started with the supervisor, and again
//! by a bridge's checks passing, and ends at once when sync is not set up or
//! automatic sync is off then: a machine that does not sync has no such
//! thread, and makes no call (R-SYNC-1). Once it has run it reads the policy
//! and the setup again each tick: off, it takes the announcement down and
//! publishes nothing until on again. It refreshes every `EVERY` with a TTL of
//! `TTL`, and takes the announcement down as the supervisor stops. Like a
//! bridge it is a background client of the daemon, so never keeps it up,
//! and a daemon that has gone is not announced.
//!
//! A refusal is said once per error code in host.log and tried again the
//! next tick: `sync_requires_paid_plan` pauses it as the supervisor pauses
//! (`Config::paid_pause`, no call meanwhile), and `device_revoked` ends it
//! until the daemon starts again.

use super::auto::Config;
use crate::catalog;
use crate::daemon::client::Client as Daemon;
use crate::instances::{default_model_of, Registry};
use crate::readiness::Report;
use crate::trust;
use krowk_client::device_chain::{refused_in_name, Chain};
use krowk_client::e2e::DeviceId;
use krowk_client::presence::{self, AnnouncedInstance, Announcement, MAX_DIRECTORIES, MAX_INSTANCES, MAX_JSON, MAX_MODELS, MAX_SESSIONS};
use krowk_client::user_key::UserKeys;
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the announcement is refreshed.
pub const EVERY: Duration = Duration::from_secs(10);
/// How long the registry lists it online past a refresh: a daemon that
/// stops answering is offline within this.
pub const TTL: u64 = 30;
const _: () = assert!(TTL >= 2 * EVERY.as_secs(), "a refresh may be missed without going offline");

/// How often the device list is brought up to date from the registry and
/// the instances checked again: a vendor's login is not asked every tick.
pub const FRESH_EVERY: Duration = super::host::LIST_EVERY;

/// How soon a tick is tried again when the daemon did not answer: one
/// starting has not bound its socket yet.
const DAEMON_WAIT: Duration = Duration::from_secs(1);

/// What one announcement is sealed and sent with: the person's keys, the
/// verified device list, this device and its signed registry client.
pub struct Announcer {
    pub api: Arc<krowk_api::Client>,
    pub keys: UserKeys,
    pub chain: Chain,
    pub device: DeviceId,
}

/// Builds an `Announcer`, the list brought up to date first when asked.
pub type Keys = Box<dyn Fn(bool) -> Result<Announcer, (String, String)> + Send + Sync>;

/// What the publisher needs from its owner.
pub struct Options {
    /// How often it refreshes (`EVERY`; a test makes it short).
    pub every: Duration,
    /// The TTL each refresh asks for (`TTL`; the registry takes 10–120).
    pub ttl: u64,
    /// When the daemon started, in milliseconds since the Unix epoch.
    pub online_since_ms: i64,
    /// The keys, list, device and client, built as a bridge's are; with
    /// `true`, the device list brought up to date from the registry first.
    /// Blocking. Errors are `(code, message)`.
    pub keys: Keys,
    /// This host's instances and each one's readiness, as `krowk status`
    /// checks them. Blocking: a vendor's login may be asked.
    pub instances: Box<dyn Fn() -> (Registry, Vec<Report>) + Send + Sync>,
    /// The models.dev cache the catalog reads, if there is one.
    pub catalog: Box<dyn Fn() -> Option<Vec<u8>> + Send + Sync>,
    /// The trusted repositories: the directories announced, less those the
    /// policy opts out.
    pub trusted: trust::Store,
}

/// The models announced for an instance of `kind` under models.dev's
/// `provider`: its own default first (`default`, else its kind's), then the
/// catalog's agentic ones, newest released first, `MAX_MODELS` at most.
pub fn models(raw: Option<&[u8]>, provider: &str, kind: &str, default: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = default.or_else(|| default_model_of(kind)).map(String::from).into_iter().collect();
    let mut listed = raw.map(|r| catalog::models(r, provider)).unwrap_or_default();
    listed.retain(|m| m.agentic);
    listed.sort_by(|a, b| b.released.cmp(&a.released).then_with(|| a.id.cmp(&b.id)));
    for m in listed {
        if out.len() >= MAX_MODELS {
            break;
        }
        if !out.contains(&m.id) {
            out.push(m.id);
        }
    }
    out
}

/// Each reported instance as announced: its readiness, and for a ready
/// one its models (`models`). The registry's default model counts as the
/// instance's own when it names that instance (`instance/model`).
pub fn instances(registry: &Registry, reports: &[Report], raw: Option<&[u8]>) -> Vec<AnnouncedInstance> {
    reports
        .iter()
        .map(|r| {
            let ready = r.readiness.is_ready();
            let own = registry.default_model.as_deref().and_then(|d| d.split_once('/')).filter(|(i, _)| *i == r.instance).map(|(_, m)| m);
            let models = match registry.instances.get(&r.instance) {
                Some(inst) if ready => models(raw, &inst.provider, r.kind, own),
                _ => Vec::new(),
            };
            AnnouncedInstance { name: r.instance.clone(), kind: r.kind.into(), ready, state: r.readiness.state().into(), fix: r.fix.clone(), models, limit: None }
        })
        .collect()
}

/// `a` brought within the record's bounds before it is sealed: whatever
/// names a thing with a character the record refuses is left out (a fix
/// line has it replaced), the lists cut to their counts, and then, while
/// the record is still too long, models, then directories, then sessions
/// go, oldest first. Pure.
pub fn fit(a: &mut Announcement) {
    let clean = |s: &str| !s.chars().any(refused_in_name);
    a.version = a.version.chars().map(|c| if refused_in_name(c) { ' ' } else { c }).collect();
    a.instances.retain(|i| clean(&i.name) && clean(&i.kind) && clean(&i.state));
    a.instances.truncate(MAX_INSTANCES);
    for i in &mut a.instances {
        i.fix = i.fix.as_ref().map(|f| f.chars().map(|c| if refused_in_name(c) { ' ' } else { c }).collect());
        i.models.retain(|m| clean(m));
        i.models.truncate(MAX_MODELS);
    }
    a.directories.retain(|d| clean(d));
    a.directories.truncate(MAX_DIRECTORIES);
    a.sessions.retain(|s| clean(s));
    // Kept newest: the daemon lists them as it took them up.
    let over = a.sessions.len().saturating_sub(MAX_SESSIONS);
    a.sessions.drain(..over);
    let size = |a: &Announcement| serde_json::to_vec(a).map_or(usize::MAX, |j| j.len());
    while size(a) > MAX_JSON {
        let dropped = if let Some(i) = a.instances.iter_mut().filter(|i| !i.models.is_empty()).max_by_key(|i| i.models.len()) {
            i.models.pop().is_some()
        } else if a.directories.pop().is_some() {
            true
        } else if !a.sessions.is_empty() {
            a.sessions.remove(0);
            true
        } else {
            false
        };
        if !dropped {
            break;
        }
    }
}

/// How the publisher is told to end.
enum End {
    /// The supervisor stops: the announcement is taken down first.
    Clean,
    /// A test's crash: it ends with the announcement left to lapse.
    Crash,
}

/// The publisher's place in the supervisor: whether its thread runs, and
/// how to stop it.
#[derive(Default)]
pub(crate) struct Publisher(Mutex<Run>);

#[derive(Default)]
struct Run {
    /// Its thread's: told how to end.
    stop: Option<Sender<End>>,
    /// Said (or dropped) as its thread ends.
    done: Option<Receiver<()>>,
    /// The registry refused this device as removed: never started again
    /// by this daemon.
    revoked: bool,
    /// The supervisor is stopping: nothing starts again.
    closed: bool,
}

impl Publisher {
    fn lock(&self) -> std::sync::MutexGuard<'_, Run> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Its thread started, when the owner gave it options and none runs.
    /// Its first tick decides whether it stays.
    pub(crate) fn ensure(me: &Arc<Publisher>, c: &Arc<Config>) {
        if c.announce.is_none() {
            return;
        }
        let mut run = me.lock();
        if run.stop.is_some() || run.revoked || run.closed {
            return;
        }
        let (stop, stop_rx) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let (me2, c) = (me.clone(), c.clone());
        let spawned = std::thread::Builder::new().name("krowk-presence".into()).spawn(move || {
            let _done = done_tx;
            let revoked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| publish(&c, &stop_rx))).unwrap_or_else(|_| {
                eprintln!("krowk: this machine stopped announcing itself to your other devices: its publisher panicked");
                false
            });
            let mut run = me2.lock();
            run.stop = None;
            run.revoked |= revoked;
        });
        if spawned.is_ok() {
            run.stop = Some(stop);
            run.done = Some(done);
        }
    }

    /// Told to end: `Clean` takes the announcement down, `Crash` leaves it.
    /// Nothing starts again.
    fn end(&self, how: End) {
        let mut run = self.lock();
        run.closed = true;
        if let Some(tx) = run.stop.take() {
            let _ = tx.send(how);
        }
    }

    /// Waits up to `most` for its thread to end.
    pub(crate) fn wait(&self, most: Duration) {
        let done = self.lock().done.take();
        if let Some(done) = done {
            let _ = done.recv_timeout(most);
        }
    }

    /// Ends it as a crash would: the announcement is left to lapse.
    pub(crate) fn crash(&self) {
        self.end(End::Crash);
        self.wait(Duration::from_secs(5));
    }

    /// Ends it, taking the announcement down, without waiting.
    pub(crate) fn drop_clean(&self) {
        self.end(End::Clean);
    }
}

/// What a tick asks next.
enum Next {
    /// Another tick after this long.
    After(Duration),
    /// The thread ends; `true` when the device was refused as removed.
    Over(bool),
}

/// The publisher's state across ticks.
struct Ticks {
    /// Announced since the last take-down: one is owed on stop or off.
    published: bool,
    /// The error codes said since the last success.
    said: HashSet<String>,
    /// When the list and the instances were last read fresh.
    fresh_at: Option<Instant>,
    instances: Vec<AnnouncedInstance>,
    /// No call until then: the workspace has no paid plan.
    paused: Option<Instant>,
    first: bool,
    daemon: Option<Daemon>,
}

/// The thread: a tick, then a wait for the next or for the end. Answers
/// whether the registry refused the device as removed.
fn publish(c: &Arc<Config>, stop: &Receiver<End>) -> bool {
    let Some(o) = c.announce.as_ref() else { return false };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        eprintln!("krowk: this machine cannot announce itself to your other devices: no async runtime for it");
        return false;
    };
    let mut t = Ticks { published: false, said: HashSet::new(), fresh_at: None, instances: Vec::new(), paused: None, first: true, daemon: None };
    let mut last: Option<Announcer> = None;
    loop {
        let next = tick(c, o, &rt, &mut t, &mut last);
        t.first = false;
        let wait = match next {
            Next::After(d) => d,
            Next::Over(revoked) => return revoked,
        };
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(End::Crash) => return false,
            Ok(End::Clean) | Err(RecvTimeoutError::Disconnected) => {
                take_down(&mut t, &last);
                return false;
            }
        }
    }
}

/// One refresh: the policy and the setup read, the record gathered, sealed
/// and sent.
fn tick(c: &Config, o: &Options, rt: &tokio::runtime::Runtime, t: &mut Ticks, last: &mut Option<Announcer>) -> Next {
    if t.paused.is_some_and(|until| Instant::now() < until) {
        return Next::After(o.every);
    }
    t.paused = None;
    let policy = match (c.policy)() {
        Ok(p) if p.on => (c.ready)().map(|()| p),
        Ok(_) => Err("automatic sync is off".into()),
        Err(e) => Err(e),
    };
    let Ok(policy) = policy else {
        // Off, or not set up: taken down, and nothing until on again — or,
        // never having run, no thread at all.
        take_down(t, last);
        return if t.first { Next::Over(false) } else { Next::After(o.every) };
    };
    let fresh = t.fresh_at.is_none_or(|at| at.elapsed() >= FRESH_EVERY);
    let a = match (o.keys)(fresh) {
        Ok(a) => a,
        Err((code, message)) => return refused(c, o, t, &code, &message),
    };
    if fresh {
        t.fresh_at = Some(Instant::now());
        let (registry, reports) = (o.instances)();
        t.instances = instances(&registry, &reports, (o.catalog)().as_deref());
    }
    let Some(me) = a.chain.devices().iter().find(|d| d.id() == a.device) else {
        return refused(c, o, t, "device_revoked", "this device is not on your device list");
    };
    let Some(sessions) = daemon_sessions(c, rt, &mut t.daemon) else {
        // A daemon that has gone is not announced; one starting is soon.
        *last = Some(a);
        return Next::After(o.every.min(DAEMON_WAIT));
    };
    let directories = o.trusted.directories().into_iter().filter(|d| policy.opted_out(d, c.krowk_home.as_deref()).is_none()).map(|d| d.display().to_string()).collect();
    let mut record = Announcement { device: a.device.to_string(), name: me.name.clone(), os: me.os.clone(), version: c.version.clone(), online_since_ms: o.online_since_ms, instances: t.instances.clone(), directories, sessions };
    fit(&mut record);
    let sent = presence::seal(&record, &a.keys, a.chain.generation(), &a.device).map_err(|e| ("announcement_unsealed".to_string(), e)).and_then(|sealed| a.api.put_presence(&sealed, o.ttl).map_err(|e| (e.code().to_string(), e.to_string())));
    *last = Some(a);
    match sent {
        Ok(_) => {
            t.published = true;
            t.said.clear();
            Next::After(o.every)
        }
        Err((code, message)) => refused(c, o, t, &code, &message),
    }
}

/// The sessions the daemon holds, asked over a background connection kept
/// across ticks; none when it does not answer.
fn daemon_sessions(c: &Config, rt: &tokio::runtime::Runtime, daemon: &mut Option<Daemon>) -> Option<Vec<String>> {
    rt.block_on(async {
        if daemon.as_ref().is_none_or(Daemon::closed) {
            *daemon = Daemon::connect_as(&c.socket, Path::new("/"), &c.version, false, true).await.ok();
        }
        let status = daemon.as_ref()?.status().await.ok();
        if status.is_none() {
            *daemon = None;
        }
        Some(status?.sessions.into_iter().map(|s| s.session_id).collect())
    })
}

/// The announcement taken down, if one is up.
fn take_down(t: &mut Ticks, last: &Option<Announcer>) {
    if let (true, Some(a)) = (std::mem::take(&mut t.published), last) {
        let _ = a.api.delete_presence();
    }
}

/// A refusal: said once per code, and what comes next for it.
fn refused(c: &Config, o: &Options, t: &mut Ticks, code: &str, message: &str) -> Next {
    let next = match code {
        "sync_requires_paid_plan" => {
            t.paused = Some(Instant::now() + c.paid_pause);
            Next::After(o.every)
        }
        "device_revoked" => Next::Over(true),
        _ => Next::After(o.every),
    };
    if t.said.insert(code.to_string()) {
        let then = match next {
            Next::Over(_) => "it stops announcing until the host daemon starts again".to_string(),
            _ if code == "sync_requires_paid_plan" => "it tries again in an hour".to_string(),
            _ => "it tries again shortly".to_string(),
        };
        eprintln!("krowk: this machine could not announce itself to your other devices: {message} ({code}); {then}");
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Announcement {
        Announcement { device: "d".into(), name: "n".into(), os: "linux".into(), version: "1\n".into(), online_since_ms: 0, instances: Vec::new(), directories: Vec::new(), sessions: Vec::new() }
    }

    fn instance(name: &str, models: usize) -> AnnouncedInstance {
        AnnouncedInstance { name: name.into(), kind: "anthropic-api".into(), ready: true, state: "ready".into(), fix: Some("a\u{202e}b".into()), models: (0..models).map(|i| format!("model-{i:03}")).collect(), limit: None }
    }

    /// R-HOST-3: the record is brought within its bounds before it is
    /// sealed — a name the record would refuse left out, models first,
    /// then directories, then the oldest sessions — and so always seals.
    #[test]
    fn r_host_3_an_announcement_is_trimmed_to_its_bounds_models_first() {
        let mut a = record();
        a.instances = vec![instance("anthropic", 40), instance("bad\nname", 1)];
        a.directories = (0..100).map(|i| format!("/work/{}/{i}", "d".repeat(150))).chain(["/evil\u{202e}".into()]).collect();
        a.sessions = (0..300).map(|i| format!("{i:036}")).collect();
        fit(&mut a);
        a.check().unwrap();
        assert_eq!(a.version, "1 ");
        assert_eq!(a.instances.len(), 1, "the instance whose name is refused is left out");
        assert_eq!(a.instances[0].fix.as_deref(), Some("a b"));
        assert!(a.instances[0].models.is_empty(), "models go first: {:?}", a.instances[0].models);
        assert!(a.directories.len() < MAX_DIRECTORIES && a.directories.iter().all(|d| d.starts_with("/work/")), "then directories");
        assert_eq!(a.sessions.len(), MAX_SESSIONS, "sessions are not reached");
        assert_eq!(a.sessions[0], format!("{:036}", 300 - MAX_SESSIONS), "the oldest went to the count");

        let mut small = record();
        small.instances = vec![instance("anthropic", 3)];
        small.directories = vec!["/work/a".into()];
        fit(&mut small);
        assert_eq!((small.instances[0].models.len(), small.directories.len()), (3, 1), "a record within its bounds keeps everything");
    }

    /// R-HOST-3: a ready instance announces its own default model first,
    /// then the catalog's agentic models newest first, at most twenty.
    #[test]
    fn r_host_3_an_instance_announces_its_default_then_the_newest_agentic_models() {
        let mut doc = serde_json::json!({"anthropic": {"models": {}}});
        for i in 0..30 {
            doc["anthropic"]["models"][format!("claude-{i:02}")] = serde_json::json!({"family": "claude", "tool_call": true, "release_date": format!("2026-01-{:02}", i % 28 + 1)});
        }
        doc["anthropic"]["models"]["embed"] = serde_json::json!({"tool_call": false, "release_date": "2027-01-01"});
        let raw = serde_json::to_vec(&doc).unwrap();
        let got = models(Some(&raw), "anthropic", "anthropic-api", Some("claude-03"));
        assert_eq!(got.len(), MAX_MODELS);
        assert_eq!(got[0], "claude-03", "its own default first");
        assert_eq!(got[1], "claude-27", "then the newest");
        assert!(!got.contains(&"embed".to_string()), "never one that is not agentic");
        assert_eq!(got.iter().filter(|m| *m == "claude-03").count(), 1);
        assert_eq!(models(None, "anthropic", "anthropic-api", None), [default_model_of("anthropic-api").unwrap()], "no catalog: its kind's default");
    }
}
