//! The status board, published and read (R-STA-5, D6, D8, D9).
//!
//! **Publishing** rides on the presence publisher's thread
//! (`announce`): it reads the daemon's registrations as the daemon last
//! wrote them (`registry::Registry::load`, beside the socket), so the
//! daemon's own thread does nothing more for it. Whenever the board's Tasks
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
use gruntas_client::device_chain::Chain;
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
    if let Ok(s) = std::fs::read_to_string(&path)
        && status::is_statistics_id(s.trim())
    {
        return Some(s.trim().to_string());
    }
    let id = status::statistics_id();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp).ok()?;
    f.write_all(id.as_bytes()).ok()?;
    drop(f);
    std::fs::rename(&tmp, &path).ok()?;
    Some(id)
}

/// The board of the sessions registered with the daemon whose socket
/// directory is `dir`, as `a.device` publishes it now: each Task's machine
/// filled in.
pub fn board(dir: &Path, device: &DeviceId) -> Board {
    let tasks: Vec<Task> = Registry::load(dir)
        .board_tasks()
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
/// person's keys) reads them: the devices listed through the presence list,
/// each one's board fetched and opened against `chain` with `keys`. A
/// device that published none is left out; one whose board does not open
/// is listed with why. A registry that keeps no presences yet lists none.
pub fn read(api: &gruntas_api::Client, keys: &UserKeys, chain: &Chain) -> Result<Vec<Read>, gruntas_api::Error> {
    let presences = match api.list_presences() {
        Ok(p) => p,
        Err(e) if e.status == 404 => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for p in presences {
        let Some(record) = api.status_board(&p.device)? else { continue };
        let received_at = jiff::Timestamp::now();
        let board = DeviceId::parse(&p.device).ok_or_else(|| "not a device id".to_string()).and_then(|d| status::open_board(&record.sealed, &d, keys, chain));
        out.push(Read { device: p.device, online: p.online, received_at, board });
    }
    Ok(out)
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
