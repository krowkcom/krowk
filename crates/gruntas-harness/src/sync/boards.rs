//! The status board, published and read (R-STA-5, D6, D8, D9).
//!
//! **Publishing** rides on the presence publisher's thread
//! (`announce`): it reads the daemon's status and registrations as the
//! daemon last wrote them (`tasks::Tasks::read`, `registry::Registry::read`,
//! beside the socket), so the daemon's own thread does nothing more for it. Whenever the board's Tasks
//! change, and at least with every presence refresh, the board is sealed
//! (`status::seal_board`, which brings it within its budget by D8's rule)
//! and put as this device's board record, at most once every `COALESCE`;
//! the counts (`status::Counts`) go with each publish under this device's
//! statistics id, never its device id (D9).
//!
//! **Reading** (`read`) lists the person's devices through the presence
//! list, fetches each one's board and opens it against the verified device
//! list; "quiet for" is `status::quiet_for`, from the board's own publish
//! time and when it was received (D6).

use super::announce::Announcer;
use crate::daemon::registry::Registry;
use crate::daemon::tasks::Tasks;
use gruntas_client::device_chain::{Chain, Kind};
use gruntas_client::e2e::DeviceId;
use gruntas_client::status::{self, Board, Counts, Field, Task};
use gruntas_client::user_key::UserKeys;
use std::path::Path;
use std::time::{Duration, Instant};

/// The least time between two publishes of the board (D8).
pub const COALESCE: Duration = Duration::from_secs(5);
/// How often the registrations are read for a change.
pub const POLL: Duration = Duration::from_secs(1);
/// The file under gruntas's home the statistics id is kept in.
pub const STATISTICS_ID_FILE: &str = "status-statistics-id";

/// When the board is next published: the Tasks last published, when, and
/// whether a presence refresh owes one.
#[derive(Debug, Default)]
pub struct Coalescer {
    sent: Option<Vec<Task>>,
    sent_at: Option<Instant>,
    owed: bool,
}

impl Coalescer {
    /// A presence refresh went out: a board is owed with it.
    pub fn refreshed(&mut self) {
        self.owed = true;
    }

    /// Whether `tasks` is published `now`: they changed since the last
    /// publish, or one is owed, and the last was at least `COALESCE` ago.
    pub fn due(&self, tasks: &[Task], now: Instant) -> bool {
        let wanted = self.owed || self.sent.as_deref() != Some(tasks);
        wanted && self.sent_at.is_none_or(|at| now.saturating_duration_since(at) >= COALESCE)
    }

    /// `tasks` was published at `now`.
    pub fn sent(&mut self, tasks: Vec<Task>, now: Instant) {
        self.sent = Some(tasks);
        self.sent_at = Some(now);
        self.owed = false;
    }

    /// Tried at `now` and refused: tried again no sooner than `COALESCE`,
    /// still owed.
    pub fn failed(&mut self, now: Instant) {
        self.sent_at = Some(now);
        self.owed = true;
    }
}

/// This device's statistics id (D9): read from `home`'s
/// `STATISTICS_ID_FILE`, or drawn (`status::statistics_id`) and written
/// there, `0600`, on first need. None without a home, or when the file can
/// neither be read nor written: the caller then keeps one for its run.
pub fn statistics_id(home: Option<&Path>) -> Option<String> {
    let path = home?.join(STATISTICS_ID_FILE);
    let kept = |path: &Path| std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| status::is_statistics_id(s));
    if let Some(id) = kept(&path) {
        return Some(id);
    }
    // One that does not read is replaced; one another process writes
    // meanwhile wins, so a machine sends under one id.
    let id = status::statistics_id();
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = write_new(&tmp, id.as_bytes()).and_then(|()| match std::fs::hard_link(&tmp, &path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match kept(&path) {
            Some(_) => Ok(()),
            None => std::fs::rename(&tmp, &path),
        },
        other => other,
    });
    let _ = std::fs::remove_file(&tmp);
    written.ok()?;
    kept(&path)
}

/// `bytes` written to a new file at `path`, `0600`, never through a link,
/// and synced.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The board of the sessions registered with the daemon whose socket
/// directory is `dir`, as `a.device` publishes it now: each Task's machine
/// filled in.
pub fn board(dir: &Path, device: &DeviceId) -> Board {
    // The daemon's Tasks (`daemon::tasks`: states, observed or reported,
    // health, git context), with what its agent's reports said (SL03: the
    // summary, history, phase and PR); and for a session registered since
    // they were last written, what its registration says.
    let (kept, registry) = (Tasks::read(dir), Registry::read(dir));
    let mut tasks: Vec<Task> = kept
        .tasks()
        .map(|t| {
            let mut t = t.clone();
            if let Some(r) = registry.task(&t.id) {
                t.status.message.clone_from(&r.status.message);
                t.history.clone_from(&r.history);
                t.metadata.gruntas.phase.clone_from(&r.metadata.gruntas.phase);
                t.metadata.gruntas.pr.clone_from(&r.metadata.gruntas.pr);
            }
            t
        })
        .collect();
    tasks.extend(registry.board_tasks().into_iter().filter(|t| kept.get(&t.id).is_none()));
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    let tasks: Vec<Task> = tasks
        .into_iter()
        .map(|mut t| {
            t.metadata.gruntas.machine = Some(Field::harness(device.to_string()));
            t
        })
        .collect();
    Board { device: device.to_string(), published_at: jiff::Timestamp::now().to_string(), sessions: tasks.iter().map(|t| t.id.clone()).collect(), tasks }
}

/// One check of the board, `c` deciding: published when due, with its
/// counts under `statistics_id`. An error is `(code, message)`.
pub fn tick(c: &mut Coalescer, dir: &Path, a: &Announcer, statistics_id: &str) -> Result<(), (String, String)> {
    let b = board(dir, &a.device);
    let now = Instant::now();
    if !c.due(&b.tasks, now) {
        return Ok(());
    }
    let sent = publish(&b, a, statistics_id);
    match &sent {
        Ok(()) => c.sent(b.tasks, now),
        Err(_) => c.failed(now),
    }
    sent
}

/// `b` sealed and put as this device's board, then its counts.
pub fn publish(b: &Board, a: &Announcer, statistics_id: &str) -> Result<(), (String, String)> {
    let sealed = status::seal_board(b, &a.keys, a.chain.generation(), &a.device).map_err(|e| ("status_board_unsealed".to_string(), e))?;
    a.api.put_status_board(&sealed).map_err(|e| (e.code(), super::announce::words(&e)))?;
    a.api.put_statistics(statistics_id, &Counts::of(b)).map_err(|e| (e.code(), super::announce::words(&e)))
}

/// One device's board as a reader received it.
#[derive(Debug)]
pub struct Read {
    /// The device, in hex.
    pub device: String,
    /// Whether its presence is live (D4: `SERVING`, else `UNKNOWN`).
    pub online: bool,
    /// When it was received, by the reader's clock (D6).
    pub received_at: jiff::Timestamp,
    /// The board opened, or why it did not open.
    pub board: Result<Board, String>,
}

/// Every board the person's devices published, as `api` (one of the
/// person's keys) reads them: the devices listed through the presence list
/// and the verified device list `chain`, so a machine whose presence lapsed
/// or was withdrawn (`host stop`) still shows, offline (D4's `UNKNOWN`);
/// each one's board fetched and opened against `chain` with `keys`. A
/// device that published none is left out; one whose board cannot be
/// fetched or does not open is listed with why.
pub fn read(api: &gruntas_api::Client, keys: &UserKeys, chain: &Chain) -> Result<Vec<Read>, gruntas_api::Error> {
    let presences = match api.list_presences() {
        Ok(p) => p,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    let devices = listed(presences.iter().map(|p| (p.device.as_str(), p.online)), chain.devices().iter().filter(|d| d.kind != Kind::Recovery).map(|d| d.id().to_string()));
    let mut out = Vec::new();
    for (device, online) in devices {
        // One device's board that cannot be fetched is listed with why.
        let record = match api.status_board(&device) {
            Ok(None) => continue,
            Ok(Some(r)) => Ok(r),
            Err(e) => Err(format!("its status board could not be fetched: {} ({})", super::announce::words(&e), e.code())),
        };
        let received_at = jiff::Timestamp::now();
        let board = record.and_then(|r| DeviceId::parse(&device).ok_or_else(|| "not a device id".to_string()).and_then(|d| status::open_board(&r.sealed, &d, keys, chain)));
        out.push(Read { device, online, received_at, board });
    }
    Ok(out)
}

/// The devices to read, each once, with whether its presence is live: the
/// presences, then each device on the verified list with none, offline.
fn listed<'a>(presences: impl Iterator<Item = (&'a str, bool)>, verified: impl Iterator<Item = String>) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for (device, online) in presences {
        if !out.iter().any(|(d, _)| d.eq_ignore_ascii_case(device)) {
            out.push((device.to_string(), online));
        }
    }
    for device in verified {
        if !out.iter().any(|(d, _)| d.eq_ignore_ascii_case(&device)) {
            out.push((device, false));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::status::{TaskMetadata, TaskState, TaskStatus};

    fn task(id: &str) -> Task {
        Task { id: id.into(), status: TaskStatus { state: TaskState::Working, message: None, timestamp: "2026-11-23T12:00:00Z".into() }, history: Vec::new(), metadata: TaskMetadata::default() }
    }

    /// SL05 (D8): the first board goes at once; a burst of changes after
    /// it goes once `COALESCE` has passed, as one publish; a presence
    /// refresh owes one even with no change, within the same bound.
    #[test]
    fn sl05_changes_coalesce_to_one_publish_every_five_seconds() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        assert!(c.due(&[], t0), "the first board goes at once");
        c.sent(Vec::new(), t0);
        assert!(!c.due(&[], t0 + COALESCE * 3), "nothing changed, nothing owed");
        let mut published = 0;
        let mut tasks = Vec::new();
        // Twenty changes in a second, then a poll each 100 ms for 6 s.
        for i in 0..60u32 {
            let now = t0 + Duration::from_millis(100 * u64::from(i));
            if i < 20 {
                tasks.push(task(&format!("s{i}")));
            }
            if c.due(&tasks, now) {
                assert!(now.duration_since(t0) >= COALESCE, "never within 5 s of the last");
                c.sent(tasks.clone(), now);
                published += 1;
            }
        }
        assert_eq!(published, 1, "the burst went as one publish");
        let at = t0 + Duration::from_secs(6);
        c.refreshed();
        assert!(!c.due(&tasks, at), "owed, but the last was within 5 s");
        assert!(c.due(&tasks, at + COALESCE), "owed, and due once 5 s passed");
        c.failed(at + COALESCE);
        assert!(!c.due(&tasks, at + COALESCE + POLL) && c.due(&tasks, at + COALESCE * 2), "a refused publish is owed, tried again 5 s on");
    }

    /// SL04: the board carries the daemon's observed states, and a session
    /// registered since they were last written as its registration says.
    #[test]
    fn sl04_the_board_is_the_daemons_tasks_then_the_registrations() {
        use crate::daemon::{registry, tasks};
        use crate::protocol::{Activity, PluginHarness, Registration};
        let dir = std::env::temp_dir().join(format!("gruntas-board-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let reg = |id: &str| Activity::Register(Registration { session_id: id.into(), harness: PluginHarness::Claude, harness_pid: None, config_home: None, cwd: None, repository: None, transcript_path: None, attendance: None });
        let mut t = tasks::Tasks::default();
        t.apply(&reg("kept"), "2026-11-23T12:00:00Z");
        t.apply(&Activity::PermissionRequest { session_id: "kept".into() }, "2026-11-23T12:00:01Z");
        t.save(&dir).unwrap();
        let mut r = registry::Registry::default();
        r.apply(&reg("kept"));
        r.apply(&reg("new"));
        r.save(&dir).unwrap();
        r.report("kept", "the summary", &gruntas_client::status::Report::default(), jiff::Timestamp::now());
        r.save(&dir).unwrap();
        let b = board(&dir, &DeviceId([7; 16]));
        let states: Vec<(&str, TaskState)> = b.tasks.iter().map(|t| (t.id.as_str(), t.status.state)).collect();
        assert_eq!(states, [("kept", TaskState::InputRequired), ("new", TaskState::Working)]);
        assert_eq!(b.tasks[0].status.message.as_ref().map(|m| m.summary()).as_deref(), Some("the summary"), "the report's summary rides on the observed state");
        assert_eq!(b.tasks[0].history.len(), 1);
        assert_eq!(b.sessions, ["kept", "new"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SL07 (D4): a verified device with no presence (lapsed, or withdrawn
    /// by `host stop`) is still read, offline; each device once.
    #[test]
    fn sl07_a_device_without_a_presence_is_read_offline() {
        let presences = [("AA", true), ("bb", false)];
        let verified = ["aa", "cc"].map(String::from);
        assert_eq!(listed(presences.into_iter(), verified.into_iter()), [("AA".to_string(), true), ("bb".to_string(), false), ("cc".to_string(), false)]);
    }

    #[test]
    fn sl05_the_statistics_id_is_drawn_once_and_kept() {
        let home = std::env::temp_dir().join(format!("gruntas-statistics-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let id = statistics_id(Some(&home)).unwrap();
        assert!(status::is_statistics_id(&id));
        assert_eq!(statistics_id(Some(&home)), Some(id));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(home.join(STATISTICS_ID_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(home.join(STATISTICS_ID_FILE), "not one").unwrap();
        assert!(statistics_id(Some(&home)).is_some_and(|id| status::is_statistics_id(&id)), "one that does not read is drawn again");
        assert_eq!(statistics_id(None), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
