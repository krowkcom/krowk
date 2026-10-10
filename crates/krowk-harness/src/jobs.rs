//! A session's background commands (R-STEER-2): each a command started as
//! `bash` starts one (`tools::start`: the sandbox plan, the environment,
//! its own process group, stdout and stderr on one pipe), or a foreground
//! call adopted as it runs, then read by the session instead of the call.
//!
//! The host keeps one table for every session it serves, so a job outlives
//! the turn that started it. Jobs are `b1`, `b2`, … per session, at most
//! `MAX_RUNNING` running at once; what each prints is appended to
//! `jobs/<id>.out` in the session's directory, up to `MAX_OUTPUT`, then one
//! truncation line. Each keeps where the last read stopped, its status and
//! when it ended. When one exits, the sandbox's sweep for it runs (once
//! everything it ran is gone, as a foreground call's does), and a note of
//! krowk's goes on the session's running turn's steer queue, or is held
//! for the session's next turn. Its process group is killed by `kill`, and
//! with the table: when the host lets go of it, or shuts down.

use crate::engine::{Steer, Steers};
use crate::tools::{Capture, Started, BASH_DRAIN_AFTER_EXIT, BASH_MAX_OUTPUT};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

/// How many of a session's jobs may run at once.
pub const MAX_RUNNING: usize = 8;
/// How much of a job's output its file keeps.
pub const MAX_OUTPUT: u64 = 8 << 20;
/// The line its file ends with when there was more.
pub const TRUNCATED: &str = "\n[krowk: output past 8 MiB was dropped]\n";
/// How many of its last lines a job's note carries.
const NOTE_LINES: usize = 20;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    Killed,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Running => f.write_str("running"),
            Status::Exited(n) => write!(f, "exited {n}"),
            Status::Killed => f.write_str("killed"),
        }
    }
}

/// Every session's jobs. Dropped, it kills what still runs.
pub struct Jobs {
    sessions_dir: PathBuf,
    table: Arc<Mutex<Table>>,
}

#[derive(Default)]
struct Table {
    sessions: HashMap<String, Session>,
}

#[derive(Default)]
struct Session {
    next: u32,
    jobs: Vec<Job>,
    /// The running turn's steer queue, where a note goes.
    turn: Option<Steers>,
    /// Notes for the session's next turn, oldest first.
    held: Vec<Steer>,
}

struct Job {
    id: String,
    file: PathBuf,
    /// Where the last read stopped, in bytes of its file.
    read: u64,
    status: Status,
    ended_ms: Option<i64>,
    kill: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

fn lock(t: &Mutex<Table>) -> std::sync::MutexGuard<'_, Table> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

impl Jobs {
    pub fn new(sessions_dir: PathBuf) -> Jobs {
        Jobs { sessions_dir, table: Arc::default() }
    }

    /// Whether `session` may start another job; the refusal says why.
    pub fn admit(&self, session: &str) -> Result<(), String> {
        admitted(&lock(&self.table), session)
    }

    /// Takes a started command on as a job of `session`, and returns its id.
    /// Refused, the command is stopped with its group.
    pub fn start(&self, session: &str, started: Started) -> Result<String, String> {
        self.take(session, started, Vec::new())
    }

    /// Takes a foreground call on as a job, as it runs (R-STEER-4): what it
    /// printed so far, `shown`, is the file's start and counts as read.
    pub fn adopt(&self, session: &str, started: Started, shown: Vec<u8>) -> Result<String, String> {
        self.take(session, started, shown)
    }

    fn take(&self, session: &str, started: Started, shown: Vec<u8>) -> Result<String, String> {
        let mut t = lock(&self.table);
        admitted(&t, session)?;
        let s = t.sessions.entry(session.to_string()).or_default();
        s.next += 1;
        let id = format!("b{}", s.next);
        let file = self.sessions_dir.join(session).join("jobs").join(format!("{id}.out"));
        let (kill, killed) = oneshot::channel();
        let read = shown.len() as u64;
        let task = tokio::spawn(watch(self.table.clone(), (session.to_string(), id.clone()), file.clone(), started, shown, killed));
        s.jobs.push(Job { id: id.clone(), file, read, status: Status::Running, ended_ms: None, kill: Some(kill), task: Some(task) });
        Ok(id)
    }

    /// What `session`'s job `id` printed since the last read, cut as `bash`
    /// cuts a call's output, and where it is.
    pub fn read(&self, session: &str, id: &str) -> Result<(String, Status), String> {
        let (file, from, status) = {
            let t = lock(&self.table);
            let j = find(&t, session, id)?;
            (j.file.clone(), j.read, j.status)
        };
        let mut bytes = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&file) {
            let _ = f.seek(std::io::SeekFrom::Start(from)).and_then(|_| f.read_to_end(&mut bytes));
        }
        // A character the job is still writing is read next time, whole.
        if status == Status::Running
            && let Err(e) = std::str::from_utf8(&bytes)
            && e.error_len().is_none()
        {
            bytes.truncate(e.valid_up_to());
        }
        if let Some(j) = lock(&self.table).sessions.get_mut(session).and_then(|s| s.jobs.iter_mut().find(|j| j.id == id)) {
            j.read = from + bytes.len() as u64;
        }
        let mut cap = Capture::new(BASH_MAX_OUTPUT);
        cap.push(&bytes);
        Ok((cap.render(), status))
    }

    /// Stops `session`'s job `id` with its whole process group.
    pub fn kill(&self, session: &str, id: &str) -> Result<(), String> {
        let mut t = lock(&self.table);
        find(&t, session, id)?;
        let j = t.sessions.get_mut(session).and_then(|s| s.jobs.iter_mut().find(|j| j.id == id)).expect("found above");
        if let Some(k) = j.kill.take() {
            let _ = k.send(());
        }
        Ok(())
    }

    /// When `session`'s job `id` ended, in milliseconds since the epoch.
    pub fn ended_ms(&self, session: &str, id: &str) -> Option<i64> {
        find(&lock(&self.table), session, id).ok().and_then(|j| j.ended_ms)
    }

    /// How many of `session`'s jobs run.
    pub fn running(&self, session: &str) -> usize {
        lock(&self.table).sessions.get(session).map_or(0, |s| s.jobs.iter().filter(|j| j.status == Status::Running).count())
    }

    /// The sessions with a job running.
    pub fn busy_sessions(&self) -> Vec<String> {
        lock(&self.table).sessions.iter().filter(|(_, s)| s.jobs.iter().any(|j| j.status == Status::Running)).map(|(id, _)| id.clone()).collect()
    }

    /// A turn of `session` starts: notes go on its queue from now on, and
    /// the ones held for it go there first.
    pub fn attach(&self, session: &str, steers: &Steers) {
        let mut t = lock(&self.table);
        let s = t.sessions.entry(session.to_string()).or_default();
        for note in std::mem::take(&mut s.held) {
            if let Err(note) = steers.push(note) {
                s.held.push(note);
            }
        }
        s.turn = Some(steers.clone());
    }

    /// `session`'s turn is over: the notes it never read wait for the next.
    pub fn detach(&self, session: &str, unread: Vec<Steer>) {
        let mut t = lock(&self.table);
        let Some(s) = t.sessions.get_mut(session) else { return };
        s.turn = None;
        s.held.splice(0..0, unread);
    }

    /// Kills every job and waits for each to be swept, as a kill does;
    /// one that takes longer than `SHUTDOWN_GRACE` is let go of as a drop
    /// lets go of it.
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = lock(&self.table).sessions.values_mut().flat_map(|s| s.jobs.iter_mut()).filter_map(|j| Some((j.kill.take(), j.task.take()?))).collect();
        let mut tasks = Vec::new();
        for (kill, task) in jobs {
            if let Some(k) = kill {
                let _ = k.send(());
            }
            tasks.push(task);
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for mut t in tasks {
            if tokio::time::timeout_at(deadline, &mut t).await.is_err() {
                t.abort();
            }
        }
    }
}

impl Drop for Jobs {
    /// Each job's task is stopped, which drops its command armed: its
    /// group is killed, and the sandbox swept once it is gone.
    fn drop(&mut self) {
        for j in lock(&self.table).sessions.values_mut().flat_map(|s| s.jobs.iter_mut()) {
            if let Some(t) = j.task.take() {
                t.abort();
            }
        }
    }
}

/// How long a shutdown waits for every job to be killed and swept: the
/// sandbox's first process gets ten seconds to go.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

/// Whether `session` may start another job; the refusal says why.
fn admitted(t: &Table, session: &str) -> Result<(), String> {
    let running = t.sessions.get(session).map_or(0, |s| s.jobs.iter().filter(|j| j.status == Status::Running).count());
    if running >= MAX_RUNNING {
        return Err(format!("not started: {MAX_RUNNING} background jobs are already running in this session, the most there can be — wait for one to end, or stop one with kill_bash"));
    }
    Ok(())
}

fn find<'a>(t: &'a Table, session: &str, id: &str) -> Result<&'a Job, String> {
    let jobs = t.sessions.get(session).map(|s| s.jobs.as_slice()).unwrap_or_default();
    jobs.iter().find(|j| j.id == id).ok_or_else(|| match jobs.len() {
        0 => format!("there is no background job {id:?}: this session has none"),
        _ => format!("there is no background job {id:?}: this session's are {}", jobs.iter().map(|j| format!("{} ({})", j.id, j.status)).collect::<Vec<_>>().join(", ")),
    })
}

/// A job's output file, appended to up to `MAX_OUTPUT`: a pipe's read at
/// a time, at most 8 KiB, which the page cache takes at once.
struct Out {
    file: Option<std::fs::File>,
    written: u64,
    truncated: bool,
}

impl Out {
    fn open(path: &Path) -> Out {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A job of the same id from before the host started again is
        // another job: its file is replaced, never read on into.
        let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path).ok();
        Out { file, written: 0, truncated: false }
    }

    fn push(&mut self, b: &[u8]) {
        let Some(f) = self.file.as_mut() else { return };
        if self.truncated {
            return;
        }
        let room = (MAX_OUTPUT - self.written) as usize;
        let keep = &b[..b.len().min(room)];
        let _ = f.write_all(keep);
        self.written += keep.len() as u64;
        if keep.len() < b.len() {
            let _ = f.write_all(TRUNCATED.as_bytes());
            self.truncated = true;
        }
    }
}

/// Reads a job's output into its file until it is done, or killed; then
/// sweeps for it, records how it ended, and leaves its note.
async fn watch(table: Arc<Mutex<Table>>, (session, id): (String, String), path: PathBuf, mut started: Started, shown: Vec<u8>, mut kill: oneshot::Receiver<()>) {
    // Bound first, so it is let go of last: after the group is killed.
    let slot = started.slot.take();
    let Started { mut child, out: mut so, err: mut se, mut group, slot: _, slot_note: _ } = started;
    let mut out = Out::open(&path);
    out.push(&shown);
    let (mut b1, mut b2) = ([0u8; 8192], [0u8; 8192]);
    let (mut so_open, mut se_open) = (true, se.is_some());
    let mut code = None;
    let mut exited = false;
    let mut killed = false;
    // As a foreground call: reading stops a moment after the shell exits,
    // so what it left running in the background does not hold the job.
    let mut drain_until: Option<tokio::time::Instant> = None;
    while so_open || se_open || !exited {
        let drained = crate::engine::sleep_until(drain_until);
        tokio::select! {
            r = so.read(&mut b1), if so_open => match r {
                Ok(n) if n > 0 => out.push(&b1[..n]),
                _ => so_open = false,
            },
            r = async {
                match se.as_mut() {
                    Some(se) => se.read(&mut b2).await,
                    None => std::future::pending().await,
                }
            }, if se_open => match r {
                Ok(n) if n > 0 => out.push(&b2[..n]),
                _ => se_open = false,
            },
            s = child.wait(), if !exited => {
                exited = true;
                code = s.ok().and_then(|s| s.code());
                drain_until = Some(tokio::time::Instant::now() + BASH_DRAIN_AFTER_EXIT);
            }
            // A kill once the shell has exited is too late to say it was
            // killed, and its group is no longer its own to signal.
            _ = &mut kill, if !killed && !exited => {
                killed = true;
                group.kill_group();
            }
            _ = drained => break,
        }
    }
    drop(out);
    // The sweep, once everything it ran is gone: killed, after the
    // sandbox's first process has exited; finished, bubblewrap's exit
    // already says so. Off the runtime, as it walks the workspace.
    if !killed {
        group.group = None;
    }
    let removed = tokio::task::spawn_blocking(move || {
        if killed {
            return group.settle();
        }
        group.unfenced.as_mut().map(|u| u.appeared()).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    drop(slot);
    let status = match code {
        Some(c) if !killed => Status::Exited(c),
        _ => Status::Killed,
    };
    let mut last = last_lines(&path);
    // What the sweep removed, as a foreground call says it: the job ran
    // beside everything else in the workspace while it ran.
    if !removed.is_empty() {
        let names = removed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
        last += &format!("the sandbox removed {names}, which appeared in the workspace while the job ran: git, Claude Code, Codex and krowk run what such a directory names\n");
    }
    let note = Steer::note(note(&id, status, &last));
    let mut t = lock(&table);
    let Some(s) = t.sessions.get_mut(&session) else { return };
    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
        j.status = status;
        j.ended_ms = Some(krowk_store::now_ms());
        j.kill = None;
    }
    let note = match &s.turn {
        Some(steers) => steers.push(note).err(),
        None => Some(note),
    };
    s.held.extend(note);
}

/// A job's end, as the model reads it: krowk's words, not the person's.
pub fn note(id: &str, status: Status, last: &str) -> String {
    format!("<background-done id=\"{id}\" status=\"{status}\">\n{last}</background-done>")
}

/// How much of a job's last lines its note carries at most.
const NOTE_BYTES: usize = 4000;

/// The last `NOTE_LINES` lines of a job's file, each ending in a newline,
/// at most `NOTE_BYTES` of them.
fn last_lines(path: &Path) -> String {
    let mut tail = Vec::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(std::io::SeekFrom::Start(len.saturating_sub(64 << 10))).and_then(|_| f.read_to_end(&mut tail));
    }
    let text = String::from_utf8_lossy(&tail);
    let lines: Vec<&str> = text.lines().collect();
    let last: String = lines[lines.len().saturating_sub(NOTE_LINES)..].iter().map(|l| format!("{l}\n")).collect();
    let cut = last.len().saturating_sub(NOTE_BYTES);
    let cut = (cut..=last.len()).find(|i| last.is_char_boundary(*i)).unwrap_or(last.len());
    last[cut..].to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocol::PermissionMode;
    use crate::tools::ToolEnv;
    use crate::toolset::EditTool;
    use std::time::Duration;

    const S: &str = "s-1";

    fn jobs(name: &str) -> (Jobs, PathBuf) {
        let d = crate::tools::tests::dir(&format!("jobs-{name}"));
        (Jobs::new(d.join("sessions")), d)
    }

    async fn start(jobs: &Jobs, cwd: &Path, command: &str) -> String {
        let env = ToolEnv { cwd, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None, builds: None, live: None, env: &[] };
        jobs.start(S, crate::tools::start(command, &env, None).await.unwrap()).unwrap()
    }

    async fn ended(jobs: &Jobs, id: &str) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while jobs.ended_ms(S, id).is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the job ended");
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn r_steer_2_a_job_is_read_in_two_parts_with_no_line_lost_or_repeated() {
        let (jobs, d) = jobs("two-reads");
        let id = start(&jobs, &d, "for i in 1 2 3; do echo line$i; done; sleep 0.5; for i in 4 5 6; do echo line$i; done").await;
        assert_eq!(id, "b1");
        let mut first = String::new();
        while !first.contains("line3") {
            let (text, status) = jobs.read(S, &id).unwrap();
            first += &text;
            assert_eq!(status, Status::Running, "{first}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ended(&jobs, &id).await;
        let (second, status) = jobs.read(S, &id).unwrap();
        assert_eq!(status, Status::Exited(0));
        assert_eq!(first + &second, "line1\nline2\nline3\nline4\nline5\nline6\n");
        assert_eq!(jobs.read(S, &id).unwrap(), (String::new(), Status::Exited(0)), "nothing new");
        assert!(jobs.read(S, "b9").unwrap_err().contains("this session's are b1 (exited 0)"));
    }

    /// The host going away kills each job's whole process group: what the
    /// command started in the background goes too.
    #[tokio::test]
    async fn r_steer_2_letting_go_of_the_host_kills_a_jobs_whole_group() {
        let (jobs, d) = jobs("let-go");
        let id = start(&jobs, &d, "sleep 300 & echo $!; wait").await;
        let mut out = String::new();
        while !out.ends_with('\n') {
            out += &jobs.read(S, &id).unwrap().0;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: i32 = out.trim().parse().unwrap();
        assert!(alive(pid));
        drop(jobs);
        tokio::time::timeout(Duration::from_secs(5), async {
            while alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the background sleep was killed with its group");
    }

    #[tokio::test]
    async fn r_steer_2_a_ninth_running_job_is_refused_and_kill_stops_one() {
        let (jobs, d) = jobs("ninth");
        for _ in 0..MAX_RUNNING {
            start(&jobs, &d, "sleep 30").await;
        }
        let refused = jobs.admit(S).unwrap_err();
        assert!(refused.contains("8 background jobs are already running"), "{refused}");
        jobs.kill(S, "b3").unwrap();
        ended(&jobs, "b3").await;
        assert_eq!(jobs.read(S, "b3").unwrap().1, Status::Killed);
        assert_eq!(jobs.running(S), MAX_RUNNING - 1);
        jobs.admit(S).unwrap();
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn r_steer_2_output_past_8_mib_stops_at_one_truncation_line() {
        let (jobs, d) = jobs("cap");
        let id = start(&jobs, &d, "head -c 9000000 /dev/zero | tr '\\0' a").await;
        ended(&jobs, &id).await;
        let file = std::fs::read(d.join("sessions").join(S).join("jobs/b1.out")).unwrap();
        assert_eq!(file.len() as u64, MAX_OUTPUT + TRUNCATED.len() as u64);
        assert!(file.ends_with(TRUNCATED.as_bytes()));
        assert!(file[..MAX_OUTPUT as usize].iter().all(|b| *b == b'a'));
    }

    /// A job that ends while a turn runs leaves one note on its queue; one
    /// that ends with no turn running is held, and goes on the next turn's
    /// queue before anything else, which the first model call drains.
    #[tokio::test]
    async fn r_steer_2_an_ended_job_leaves_one_note_on_the_running_turn_or_the_next() {
        let (jobs, d) = jobs("notes");
        let turn = Steers::default();
        jobs.attach(S, &turn);
        let id = start(&jobs, &d, "echo hi; exit 3").await;
        ended(&jobs, &id).await;
        let notes = turn.take();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].from_krowk);
        assert_eq!(notes[0].text, "<background-done id=\"b1\" status=\"exited 3\">\nhi\n</background-done>");
        jobs.detach(S, Vec::new());

        let id = start(&jobs, &d, "echo later").await;
        ended(&jobs, &id).await;
        let next = Steers::default();
        jobs.attach(S, &next);
        let held = next.take();
        assert_eq!(held.len(), 1);
        assert!(held[0].text.starts_with("<background-done id=\"b2\" status=\"exited 0\">\nlater\n"), "{}", held[0].text);
        // A note the turn did not read waits for the one after.
        jobs.detach(S, held);
        let after = Steers::default();
        jobs.attach(S, &after);
        assert_eq!(after.take().len(), 1);
    }

    /// A host started again numbers jobs from b1 again: the new b1 is its
    /// own job, never read on from the old one's file.
    #[tokio::test]
    async fn r_steer_2_a_job_after_the_host_starts_again_has_its_own_file() {
        let (jobs, d) = jobs("again");
        let id = start(&jobs, &d, "echo old").await;
        ended(&jobs, &id).await;
        drop(jobs);
        let again = Jobs::new(d.join("sessions"));
        let id = start(&again, &d, "echo new").await;
        assert_eq!(id, "b1");
        ended(&again, &id).await;
        assert_eq!(again.read(S, &id).unwrap(), ("new\n".to_string(), Status::Exited(0)));
    }

    /// A foreground call taken on as it runs (R-STEER-4): what was shown is
    /// the file's start and counts as read.
    #[tokio::test]
    async fn r_steer_2_an_adopted_call_reads_on_from_what_was_shown() {
        let (jobs, d) = jobs("adopt");
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None, builds: None, live: None, env: &[] };
        let started = crate::tools::start("sleep 0.3; echo after", &env, None).await.unwrap();
        let id = jobs.adopt(S, started, b"before\n".to_vec()).unwrap();
        ended(&jobs, &id).await;
        assert_eq!(jobs.read(S, &id).unwrap(), ("after\n".to_string(), Status::Exited(0)));
        assert_eq!(std::fs::read_to_string(d.join("sessions").join(S).join("jobs/b1.out")).unwrap(), "before\nafter\n");
    }
}
