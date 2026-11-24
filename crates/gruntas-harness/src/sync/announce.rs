//! The host daemon announces itself to the person's other devices (R-HOST-3;
//! engineering/harness.md → Sync → Hosts announce themselves): that it is
//! online, its gruntas version, its instances and their models, the
//! directories it starts sessions in and the sessions it holds — sealed to
//! the person's devices (`gruntas_client::presence`), so the registry holds
//! only bytes.
//!
//! **The publisher** runs on a thread of its own, `gruntas-presence`, beside
//! the supervisor's list reader and never on the supervisor's runtime, which
//! so gains no timer (R-PERF-2). It is started with the supervisor, and again
//! by a bridge's checks passing, and ends whenever sync is not set up here,
//! its announcement taken down: a machine that does not sync has no such
//! thread, and makes no call (R-SYNC-1). It reads the policy each tick:
//! automatic sync off, it takes the announcement down and sends nothing
//! until it is on again. It refreshes every `EVERY` with a TTL of
//! `TTL`, and takes the announcement down as the supervisor stops. Like a
//! bridge it is a background client of the daemon, so never keeps it up,
//! and a daemon that has gone is not announced.
//!
//! The same thread publishes the machine's status board (`boards`, R-STA-5):
//! checked every `boards::POLL` between refreshes, put on change and with
//! every refresh, at most once every `boards::COALESCE`, and only while
//! the announcement is up.
//!
//! A refusal is said once per error code in host.log and tried again the
//! next tick: `sync_requires_paid_plan` pauses it as the supervisor pauses
//! (`Config::paid_pause`, no call meanwhile), and this device's removal
//! (`REVOKED`) ends it until the daemon starts again.

use super::auto::Config;
use super::boards;
use super::commands::{offered, Commands};
use crate::catalog;
use crate::daemon::client::Client as Daemon;
use crate::instances::{default_model_of, Registry};
use crate::readiness::Report;
use crate::trust;
use gruntas_client::device_chain::{refused_in_name, Chain};
use gruntas_client::e2e::DeviceId;
use gruntas_client::presence::{self, AnnouncedInstance, Announcement, MAX_DIRECTORIES, MAX_INSTANCES, MAX_JSON, MAX_MODELS, MAX_REPOSITORIES, MAX_SESSIONS};
use gruntas_client::user_key::UserKeys;
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

/// How often a host that was just delivered a command refreshes, and for
/// how long after (the god agent's D8): a conversation with the god agent
/// asks again within seconds, and is answered at the next refresh.
pub const RESPONSIVE_EVERY: Duration = Duration::from_secs(2);
pub const RESPONSIVE_FOR: Duration = Duration::from_secs(120);

/// Whether the refresh is quick now: until `RESPONSIVE_FOR` after the last
/// refresh that delivered a command.
#[derive(Debug, Default, Clone, Copy)]
pub struct Responsive {
    until: Option<Instant>,
}

impl Responsive {
    /// A refresh at `now` delivered a command.
    pub fn heard(&mut self, now: Instant) {
        self.until = Some(now + RESPONSIVE_FOR);
    }

    /// How long until the next refresh at `now`: `every`, or
    /// `RESPONSIVE_EVERY` while quick, whichever is sooner.
    pub fn every(&self, every: Duration, now: Instant) -> Duration {
        if self.until.is_some_and(|until| now < until) { every.min(RESPONSIVE_EVERY) } else { every }
    }
}

/// How often the device list is brought up to date from the registry and
/// the instances checked again: a vendor's login is not asked every tick.
pub const FRESH_EVERY: Duration = super::host::LIST_EVERY;

/// How soon a tick is tried again when the daemon did not answer: one
/// starting has not bound its socket yet.
const DAEMON_WAIT: Duration = Duration::from_secs(1);

/// What one announcement is sealed and sent with: the person's keys, the
/// verified device list, this device and its signed registry client.
pub struct Announcer {
    pub api: Arc<gruntas_api::Client>,
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
    /// This host's instances and each one's readiness, as `gruntas providers list --check`
    /// checks them. Blocking: a vendor's login may be asked.
    pub instances: Box<dyn Fn() -> (Registry, Vec<Report>) + Send + Sync>,
    /// The models.dev cache the catalog reads, if there is one.
    pub catalog: Box<dyn Fn() -> Option<Vec<u8>> + Send + Sync>,
    /// The trusted repositories: the directories announced, less those the
    /// policy opts out.
    pub trusted: trust::Store,
    /// The permission mode a session another device starts here runs in
    /// (R-HOST-4): the host's own default for the directory, as `gruntas -p`
    /// with no `--permission-mode` resolves it. Never anything a command
    /// carries. Errors are `(code, message)`, and refuse the start.
    pub mode_for: ModeFor,
    /// How this host takes up a session another holds, as a `take` command
    /// asks (R-HAND-1, R-HAND-4): `gruntas sync take <session>` run in the
    /// directory, detached, as the target's own take. None takes none.
    pub take: Option<TakeUp>,
    /// The pairings this host opens when a new machine asks the person's
    /// hosts for one (R-EASE-5): handed each refresh's requests. None
    /// opens none.
    pub introductions: Option<Arc<super::introduce::Introductions>>,
}

/// The host's default permission mode for a directory.
pub type ModeFor = Box<dyn Fn(&Path) -> Result<crate::protocol::PermissionMode, (String, String)> + Send + Sync>;

/// Starts `gruntas sync take <session>` in `cwd`, detached from the daemon
/// (it hosts the session once moved, so outlives it), stdin closed, its
/// stdout and stderr to a file of its own: the child, and that file.
pub type TakeUp = Box<dyn Fn(&str, &Path) -> Result<(std::process::Child, std::path::PathBuf), String> + Send + Sync>;

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

/// This machine's capacity now (R-ALLOC-1): its cores, 1-minute load and
/// free memory from the OS, and the slots of `builds` and `agents` in use
/// (`Pool::in_use`) and in all. The build slots in use count the heavy
/// commands Claude Code and Codex sessions report too (`builds::RUNNING`),
/// never past the total. Blocking but quick. A pool that cannot be read
/// counts as full, and memory the OS does not give as none: no room, not
/// plenty. Load it does not give reads as 0.
pub fn capacity(builds: &crate::slots::Pool, agents: &crate::slots::Pool) -> presence::Capacity {
    let running = crate::builds::RUNNING.lock().unwrap_or_else(|e| e.into_inner()).count(Instant::now());
    presence::Capacity {
        cores: u32::try_from(crate::builds::cores()).unwrap_or(u32::MAX).max(1),
        load_1m: load_1m(),
        free_memory: free_memory(),
        build_slots: slots(builds.size(), builds.in_use().ok(), running),
        agent_slots: slots(agents.size(), agents.in_use().ok(), 0),
    }
}

/// A pool of `size` slots with `held` taken (none: it cannot be read) and
/// `more` in use beside them, as announced: never more in use than in all.
fn slots(size: usize, held: Option<usize>, more: usize) -> presence::Slots {
    let total = u32::try_from(size).unwrap_or(u32::MAX);
    let in_use = held.map_or(total, |n| u32::try_from(n.saturating_add(more)).unwrap_or(u32::MAX).min(total));
    presence::Slots { in_use, total }
}

/// The 1-minute load average, 0 when the OS gives none.
fn load_1m() -> f64 {
    let mut load = [0f64; 1];
    // SAFETY: getloadavg writes at most the one sample asked for.
    let got = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
    if got == 1 && load[0].is_finite() && load[0] >= 0.0 { load[0] } else { 0.0 }
}

/// Memory free for new work, in bytes: what the OS counts as available
/// (caches it can give back included), 0 when it cannot be read.
#[cfg(target_os = "linux")]
fn free_memory() -> u64 {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else { return 0 };
    let kib = |key: &str| info.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok());
    kib("MemAvailable:").or_else(|| kib("MemFree:")).map_or(0, |k| k.saturating_mul(1024))
}

#[cfg(target_os = "macos")]
fn free_memory() -> u64 {
    fn sysctl(name: &str) -> Option<u64> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated and `value` is `size` bytes long, as the call asks.
        let rc = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut value as *mut u64).cast(), &mut size, std::ptr::null_mut(), 0) };
        // A 32-bit answer fills the low half of the zeroed word on a little-endian machine.
        (rc == 0).then_some(value)
    }
    // SAFETY: sysconf has no preconditions; -1 is its failure.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let pages = ["vm.page_free_count", "vm.page_purgeable_count"].iter().filter_map(|n| sysctl(n)).fold(0u64, u64::saturating_add);
    u64::try_from(page).unwrap_or(0).saturating_mul(pages)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory() -> u64 {
    0
}

/// The allocation fields of `record` filled in before it is fitted
/// (R-ALLOC-1, R-ALLOC-12): `capacity`; `accepting` from `policy`
/// (`sync.accept`); and `repositories`, each of `record.directories` whose
/// `origin` remote normalises (`presence::normalise_origin`), a directory
/// with none left out. The publisher calls it.
pub fn fill(record: &mut Announcement, policy: &super::auto::Policy, capacity: presence::Capacity) {
    record.capacity = Some(capacity);
    record.accepting = policy.accepting;
    record.repositories = repositories(&record.directories);
}

/// The repositories among `directories`, in order, at most `MAX_REPOSITORIES`:
/// each with its `origin` remote's URL normalised, so never with a
/// credential. A directory with no origin, or a local one, is left out.
pub fn repositories(directories: &[String]) -> Vec<presence::AnnouncedRepository> {
    let mut out = Vec::new();
    for d in directories {
        if out.len() >= MAX_REPOSITORIES {
            break;
        }
        let url = gruntas_api::git::query(Path::new(d)).and_then(|mut c| c.args(["config", "--get", "remote.origin.url"]).output());
        let Some(origin) = url.ok().filter(|o| o.status.success()).and_then(|o| presence::normalise_origin(String::from_utf8_lossy(&o.stdout).trim())) else { continue };
        out.push(presence::AnnouncedRepository { directory: d.clone(), origin });
    }
    out
}

/// `a` brought within the record's bounds before it is sealed: whatever
/// names a thing with a character the record refuses is left out (a fix
/// line has it replaced), the lists cut to their counts, and then, while
/// the record is still too long, models, then directories, then sessions,
/// then repositories go: the oldest-released model, the directory last on
/// the list, the oldest session, the repository last on the list; the
/// capacity only after all of them (R-ALLOC-1). Pure.
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
    a.repositories.retain(|r| clean(&r.directory) && clean(&r.origin));
    a.repositories.truncate(MAX_REPOSITORIES);
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
            a.repositories.pop().is_some() || a.capacity.take().is_some()
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
    /// Set as it is told to end, read by a tick under way.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// Said (or dropped) as its thread ends.
    done: Option<Receiver<()>>,
    /// The registry refused this device as removed: never started again
    /// by this daemon.
    revoked: bool,
    /// The supervisor is stopping: nothing starts again.
    closed: bool,
    /// The commands taken, kept across the publisher's runs.
    commands: Arc<Commands>,
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
        let (me2, c, commands) = (me.clone(), c.clone(), run.commands.clone());
        run.stopping = Arc::default();
        let stopping = run.stopping.clone();
        let spawned = std::thread::Builder::new().name("gruntas-presence".into()).spawn(move || {
            let _done = done_tx;
            let revoked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| publish(&c, &stop_rx, stopping, &commands))).unwrap_or_else(|_| {
                eprintln!("gruntas: this machine stopped announcing itself to your other devices: its publisher panicked");
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
        run.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
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
    /// How many refreshes went out: the board is owed one with each.
    refreshes: u64,
    /// The error codes said since the last success.
    said: HashSet<String>,
    /// When the list and the instances were last read fresh.
    fresh_at: Option<Instant>,
    instances: Vec<AnnouncedInstance>,
    /// The build and agent slot pools, read with the instances.
    pools: Option<(crate::slots::Pool, crate::slots::Pool)>,
    /// No call until then: the workspace has no paid plan.
    paused: Option<Instant>,
    daemon: Option<Daemon>,
    /// Set as the publisher is told to end, so a tick under way puts nothing.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Quick refreshes after a delivered command (D8).
    responsive: Responsive,
}

impl Ticks {
    fn stopping(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The thread: a tick, then a wait for the next or for the end. Answers
/// whether the registry refused the device as removed.
fn publish(c: &Arc<Config>, stop: &Receiver<End>, stopping: Arc<std::sync::atomic::AtomicBool>, commands: &Arc<Commands>) -> bool {
    let Some(o) = c.announce.as_ref() else { return false };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        eprintln!("gruntas: this machine cannot announce itself to your other devices: no async runtime for it");
        return false;
    };
    let mut t = Ticks { published: false, refreshes: 0, said: HashSet::new(), fresh_at: None, instances: Vec::new(), pools: None, paused: None, daemon: None, stop: stopping, responsive: Responsive::default() };
    let mut last: Option<Announcer> = None;
    // The status board (R-STA-5) is checked every `boards::POLL` between
    // refreshes, and owed with each.
    let mut board = Boards { due: boards::Coalescer::default(), said: HashSet::new(), statistics_id: boards::statistics_id(c.gruntas_home.as_deref()).unwrap_or_else(gruntas_client::status::statistics_id) };
    let mut refresh_at = Instant::now();
    loop {
        if Instant::now() >= refresh_at {
            let refreshes = t.refreshes;
            match tick(c, o, &rt, &mut t, &mut last, commands) {
                Next::After(d) => refresh_at = Instant::now() + d,
                Next::Over(revoked) => return revoked,
            }
            if t.refreshes != refreshes {
                board.due.refreshed();
            }
        }
        if t.published && t.paused.is_none() && !t.stopping()
            && let Some(a) = &last
        {
            board.tick(&c.socket, a);
        }
        let wait = refresh_at.saturating_duration_since(Instant::now()).min(boards::POLL);
        // Told to stop during the tick: the word is on its way.
        let told = if t.stopping() { stop.recv().map_err(|_| RecvTimeoutError::Disconnected) } else { stop.recv_timeout(wait) };
        match told {
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
/// and sent, and the commands its answer carries taken (`commands`).
fn tick(c: &Arc<Config>, o: &Options, rt: &tokio::runtime::Runtime, t: &mut Ticks, last: &mut Option<Announcer>, commands: &Arc<Commands>) -> Next {
    if t.paused.is_some_and(|until| Instant::now() < until) {
        return Next::After(o.every);
    }
    t.paused = None;
    let policy = match (c.policy)() {
        Ok(p) if p.on => (c.ready)().map(|()| p),
        Ok(_) => Err("automatic sync is off".into()),
        Err(e) => Err(e),
    };
    let policy = match policy {
        Ok(p) => p,
        // Not set up here: taken down, and the thread ends — a machine
        // that does not sync has none. A bridge's checks passing starts it
        // again.
        Err(_) if (c.ready)().is_err() => {
            take_down(t, last);
            return Next::Over(false);
        }
        // Set up, and off: taken down, and nothing sent until on again.
        Err(_) => {
            take_down(t, last);
            return Next::After(o.every);
        }
    };
    let fresh = t.fresh_at.is_none_or(|at| at.elapsed() >= FRESH_EVERY);
    if fresh {
        // Once a minute, whether it answers or not: a list that cannot be
        // read is not asked for again every tick.
        t.fresh_at = Some(Instant::now());
    }
    let a = match (o.keys)(fresh) {
        Ok(a) => a,
        Err((code, message)) => return refused(c, o, t, &code, &message),
    };
    if fresh || t.instances.is_empty() {
        let (registry, reports) = (o.instances)();
        t.instances = instances(&registry, &reports, (o.catalog)().as_deref());
        t.pools = Some(pools_of(&registry));
    }
    let Some(me) = a.chain.devices().iter().find(|d| d.id() == a.device) else {
        return refused(c, o, t, "device_revoked", "this device is not on your device list");
    };
    let Some(sessions) = daemon_sessions(c, rt, &mut t.daemon) else {
        // A daemon that has gone is not announced; one starting is soon.
        *last = Some(a);
        return Next::After(o.every.min(DAEMON_WAIT));
    };
    let directories = offered(c, o, &policy);
    let mut record = Announcement { device: a.device.to_string(), name: me.name.clone(), os: me.os.clone(), version: c.version.clone(), online_since_ms: o.online_since_ms, instances: t.instances.clone(), directories, sessions, capacity: None, repositories: Vec::new(), accepting: false };
    let (builds, agents) = t.pools.get_or_insert_with(|| pools_of(&Registry::default()));
    fill(&mut record, &policy, capacity(builds, agents));
    fit(&mut record);
    // Told to stop while it gathered: nothing more is put, so the stop's
    // take-down is the last word.
    if t.stopping() {
        *last = Some(a);
        return Next::After(Duration::ZERO);
    }
    let sent = presence::seal(&record, &a.keys, a.chain.generation(), &a.device).map_err(|e| ("announcement_unsealed".to_string(), e)).and_then(|sealed| a.api.put_presence(&sealed, o.ttl).map_err(|e| (e.code(), words(&e))));
    if let Ok(put) = &sent
        && !put.commands.is_empty()
    {
        t.responsive.heard(Instant::now());
        Commands::take(commands, c, &a, &record.name, put.commands.clone());
    }
    if let (Ok(put), Some(i)) = (&sent, &o.introductions)
        && !put.pairing_requests.is_empty()
    {
        i.deliver(&put.pairing_requests);
    }
    *last = Some(a);
    match sent {
        Ok(_) => {
            t.published = true;
            t.refreshes += 1;
            t.said.clear();
            Next::After(t.responsive.every(o.every, Instant::now()))
        }
        Err((code, message)) => refused(c, o, t, &code, &message),
    }
}

/// The build and agent slot pools of `registry`; where it names none (a
/// config that did not read), the defaults in this process's runtime directory.
fn pools_of(registry: &Registry) -> (crate::slots::Pool, crate::slots::Pool) {
    let env = gruntas_api::home::process_env;
    let runtime = crate::slots::runtime_dir(&env);
    let builds = registry.builds.pool.clone().unwrap_or_else(|| crate::slots::Pool::new(runtime.clone(), crate::builds::POOL, (crate::builds::cores() / 4).max(1)));
    let agents = registry.agents.clone().unwrap_or_else(|| crate::slots::Pool::new(runtime, crate::children::AGENT_SLOTS, crate::children::max_host()));
    (builds, agents)
}

/// The sessions the daemon holds, asked over a background connection kept
/// across ticks; none when it does not answer.
fn daemon_sessions(c: &Config, rt: &tokio::runtime::Runtime, daemon: &mut Option<Daemon>) -> Option<Vec<String>> {
    rt.block_on(async {
        if daemon.as_ref().is_none_or(Daemon::closed) {
            *daemon = Daemon::connect_as(&c.socket, Path::new("/"), &c.version, false, true).await.ok();
        }
        // Its own process: an answer within seconds, or none this tick.
        let status = tokio::time::timeout(DAEMON_WAIT * 3, daemon.as_ref()?.status()).await.ok().and_then(Result::ok);
        if status.is_none() {
            *daemon = None;
        }
        Some(status?.sessions.into_iter().map(|s| s.session_id).collect())
    })
}

/// The status board's publishing, beside the announcement's.
struct Boards {
    due: boards::Coalescer,
    /// The error codes said since the last publish.
    said: HashSet<String>,
    statistics_id: String,
}

impl Boards {
    /// One check of the board, beside the socket's registrations; a
    /// refusal said once per code in host.log.
    fn tick(&mut self, socket: &Path, a: &Announcer) {
        match boards::tick(&mut self.due, socket.parent().unwrap_or(Path::new("/")), a, &self.statistics_id) {
            Ok(()) => {}
            Err((code, message)) => {
                if self.said.insert(code.clone()) {
                    eprintln!("gruntas: this machine could not publish its status board to your other devices: {message} ({code}); it tries again shortly");
                }
                return;
            }
        }
        self.said.clear();
    }
}

/// The announcement taken down, if one is up.
fn take_down(t: &mut Ticks, last: &Option<Announcer>) {
    if let (true, Some(a)) = (std::mem::take(&mut t.published), last) {
        let _ = a.api.delete_presence();
    }
}

/// The codes that say this device may no longer act for the person: the
/// registry's refusal of a removed device, and the device list's own words
/// for a removal or a list started over.
const REVOKED: [&str; 3] = ["device_revoked", "device_removed", "device_list_reset"];

/// How long a key the registry refuses (`unauthorized`) is left before it
/// is tried again: a key revoked with its device, or one rotated, which a
/// `gruntas login` mends with no restart.
pub const UNAUTHORIZED_PAUSE: Duration = Duration::from_secs(5 * 60);

/// A registry error in a person's words: its message, and its fix.
pub fn words(e: &gruntas_api::Error) -> String {
    let message = e.body.get("message").and_then(|m| m.as_str()).unwrap_or_default();
    match (message, e.fix()) {
        ("", f) if f.is_empty() => format!("the registry answered {}", e.status),
        (m, f) if f.is_empty() => m.to_string(),
        ("", f) => f,
        // A fix that is the registry's message whole, said once.
        (m, f) if m == f => f,
        (m, f) => format!("{m} — {f}"),
    }
}

/// A refusal: said once per code, and what comes next for it.
fn refused(c: &Config, o: &Options, t: &mut Ticks, code: &str, message: &str) -> Next {
    let next = match code {
        "sync_requires_paid_plan" => {
            t.paused = Some(Instant::now() + c.paid_pause);
            Next::After(o.every)
        }
        "unauthorized" => {
            t.paused = Some(Instant::now() + UNAUTHORIZED_PAUSE);
            Next::After(o.every)
        }
        // This device is off the list, or its key with it: it has nothing
        // to announce as until the daemon starts again.
        c if REVOKED.contains(&c) => Next::Over(true),
        _ => Next::After(o.every),
    };
    if t.said.insert(code.to_string()) {
        let then = match next {
            Next::Over(_) => "it stops announcing until the host daemon starts again".to_string(),
            _ if code == "sync_requires_paid_plan" => "it tries again in an hour".to_string(),
            _ if code == "unauthorized" => "it tries again in five minutes".to_string(),
            _ => "it tries again shortly".to_string(),
        };
        eprintln!("gruntas: this machine could not announce itself to your other devices: {message} ({code}); {then}");
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Announcement {
        Announcement { device: "d".into(), name: "n".into(), os: "linux".into(), version: "1\n".into(), online_since_ms: 0, instances: Vec::new(), directories: Vec::new(), sessions: Vec::new(), capacity: None, repositories: Vec::new(), accepting: false }
    }

    fn instance(name: &str, models: usize) -> AnnouncedInstance {
        AnnouncedInstance { name: name.into(), kind: "anthropic-api".into(), ready: true, state: "ready".into(), fix: Some("a\u{202e}b".into()), models: (0..models).map(|i| format!("model-{i:03}")).collect(), limit: None }
    }

    /// GA01 (D8): a host that was delivered a command refreshes every 2 s
    /// for the next 2 minutes.
    #[test]
    fn ga01_a_host_sent_a_command_refreshes_every_2_s_for_2_minutes() {
        let (t0, every) = (Instant::now(), EVERY);
        let mut r = Responsive::default();
        assert_eq!(r.every(every, t0), every, "no command yet: the usual 10 s");
        r.heard(t0);
        assert_eq!(r.every(every, t0), Duration::from_secs(2));
        assert_eq!(r.every(every, t0 + Duration::from_secs(119)), Duration::from_secs(2));
        assert_eq!(r.every(every, t0 + Duration::from_secs(120)), every, "two minutes on: back to 10 s");
        r.heard(t0 + Duration::from_secs(100));
        assert_eq!(r.every(every, t0 + Duration::from_secs(200)), Duration::from_secs(2), "another command keeps it quick");
        assert_eq!(r.every(Duration::from_millis(50), t0 + Duration::from_secs(200)), Duration::from_millis(50), "never slower than asked");
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

    /// R-ALLOC-1: 64 repositories, the most models and a capacity fit in
    /// one record; past that the repositories go after models, directories
    /// and sessions, and the capacity only once nothing else is left.
    #[test]
    fn r_alloc_1_the_capacity_is_the_last_thing_an_announcement_loses() {
        let capacity = presence::Capacity { cores: 16, load_1m: 15.5, free_memory: 1 << 34, build_slots: presence::Slots { in_use: 3, total: 3 }, agent_slots: presence::Slots { in_use: 2, total: 32 } };
        let repo = |i: usize| presence::AnnouncedRepository { directory: format!("/home/someone/Repositories/project-{i:02}"), origin: format!("github.com/someone/project-{i:02}") };
        let mut a = record();
        a.instances = (0..4).map(|n| instance(&format!("instance-{n}"), MAX_MODELS)).collect();
        a.directories = (0..MAX_REPOSITORIES).map(|i| repo(i).directory).collect();
        a.repositories = (0..MAX_REPOSITORIES).map(repo).collect();
        a.capacity = Some(capacity);
        a.accepting = true;
        fit(&mut a);
        a.check().unwrap();
        assert_eq!((a.repositories.len(), a.capacity), (MAX_REPOSITORIES, Some(capacity)), "64 repositories and the capacity survive");
        assert!(serde_json::to_vec(&a).unwrap().len() <= MAX_JSON);

        let mut huge = a.clone();
        huge.directories = (0..MAX_DIRECTORIES).map(|i| format!("/{}/{i}", "d".repeat(180))).collect();
        huge.sessions = (0..MAX_SESSIONS).map(|i| format!("{i:036}")).collect();
        huge.repositories = (0..MAX_REPOSITORIES).map(|i| presence::AnnouncedRepository { directory: format!("/{}/{i}", "r".repeat(180)), origin: format!("github.com/{}/{i}", "o".repeat(100)) }).collect();
        fit(&mut huge);
        huge.check().unwrap();
        assert!(huge.instances.iter().all(|i| i.models.is_empty()) && huge.directories.is_empty() && huge.sessions.is_empty(), "models, directories and sessions went first");
        assert!(huge.repositories.len() < MAX_REPOSITORIES && !huge.repositories.is_empty(), "then repositories, from the end");
        assert_eq!(huge.capacity, Some(capacity), "the capacity stays");

        let mut overflow = record();
        overflow.capacity = Some(capacity);
        overflow.version = "v".repeat(MAX_JSON);
        fit(&mut overflow);
        assert_eq!(overflow.capacity, None, "and goes only when nothing else is left");
    }

    /// Slice 02 (R-ALLOC-1, R-ALLOC-12): the record carries the capacity,
    /// `sync.accept` as `accepting`, and each directory with an origin as a
    /// repository, normalised, with no credential.
    #[test]
    fn alloc_02_the_record_carries_capacity_acceptance_and_repositories() {
        let root = std::env::temp_dir().join(format!("gruntas-fill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (app, plain) = (root.join("app"), root.join("plain"));
        for d in [&app, &plain] {
            std::fs::create_dir_all(d).unwrap();
            assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(d).status().unwrap().success());
        }
        assert!(std::process::Command::new("git").args(["remote", "add", "origin", "https://ann:ghp_secret@GitHub.com/acme/app.git"]).current_dir(&app).status().unwrap().success());
        let mut a = record();
        a.directories = vec![app.display().to_string(), plain.display().to_string()];
        let capacity = presence::Capacity { cores: 4, load_1m: 0.5, free_memory: 1 << 33, ..Default::default() };
        let off = super::super::auto::policy(&|_| String::new(), Some(&serde_json::json!({"sync": {"accept": false}}))).unwrap();
        fill(&mut a, &off, capacity);
        assert_eq!(a.capacity, Some(capacity));
        assert!(!a.accepting, "sync.accept false");
        assert_eq!(a.repositories, [presence::AnnouncedRepository { directory: app.display().to_string(), origin: "github.com/acme/app".into() }], "the directory with an origin, normalised; none without");
        let on = super::super::auto::policy(&|_| String::new(), None).unwrap();
        fill(&mut a, &on, capacity);
        assert!(a.accepting, "on by default");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KT03 (R-KIT-5): the heavy commands Claude Code and Codex report are
    /// added to the lock files' build slots, never past the total, and a
    /// pool that cannot be read still reads as full.
    #[test]
    fn kt03_reported_heavy_commands_add_to_the_busy_build_slots_up_to_the_total() {
        assert_eq!(slots(4, Some(1), 0), presence::Slots { in_use: 1, total: 4 });
        assert_eq!(slots(4, Some(1), 2), presence::Slots { in_use: 3, total: 4 });
        assert_eq!(slots(4, Some(3), 5), presence::Slots { in_use: 4, total: 4 }, "clamped to the total");
        assert_eq!(slots(4, Some(usize::MAX), usize::MAX), presence::Slots { in_use: 4, total: 4 });
        assert_eq!(slots(4, None, 0), presence::Slots { in_use: 4, total: 4 });
    }

    /// Slice 02 (R-ALLOC-1): this machine reads its own capacity, its slot
    /// totals the pools'.
    #[test]
    fn alloc_02_this_machine_reads_its_capacity() {
        let runtime = std::env::temp_dir().join(format!("gruntas-capacity-{}", std::process::id()));
        let builds = crate::slots::Pool::new(runtime.clone(), "build-slots", 3);
        let agents = crate::slots::Pool::new(runtime.clone(), crate::children::AGENT_SLOTS, 5);
        let held = agents.try_take().unwrap().expect("a slot");
        let c = capacity(&builds, &agents);
        assert!(c.cores >= 1 && c.load_1m >= 0.0 && c.free_memory > 0, "{c:?}");
        assert_eq!((c.build_slots, c.agent_slots), (presence::Slots { in_use: 0, total: 3 }, presence::Slots { in_use: 1, total: 5 }));
        drop(held);
        let _ = std::fs::remove_dir_all(&runtime);
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
