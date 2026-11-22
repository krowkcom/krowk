//! The commands another of the person's devices sends this host (R-HOST-4;
//! engineering/harness.md → Sync → Starting a session from another device),
//! delivered in each presence refresh's answer (`announce`) until the host
//! answers: `start`, a session in a directory this host announced, on a
//! model it has ready, in the host's own default permission mode for that
//! directory, whatever the command says; and `take`, a session another host
//! holds moved here (R-HAND-1, R-HAND-4).
//!
//! **Once.** The last `KEEP` ids answered are kept with the sealed answer
//! given, as the bridge keeps its command ids: a delivered id already
//! answered is answered the same again, and nothing runs. One being worked
//! on, or whose answer is on its way, is skipped until it is done. One that
//! does not open gets no answer (an answer would need a key the sender
//! proved it holds) and is said once in host.log; it is tried again a
//! minute later, as the host reads its device list again each minute,
//! since a device paired since, or a user key rotated since, is not yet on
//! the list this host holds. Those
//! are kept apart, `UNOPENED` at most, so bytes nobody can open never push
//! an answered id out.
//!
//! **Start.** On a thread of its own, so a slow start never holds the
//! refresh up, nor does an answer sent: the directory checked against what
//! this host offers, read again now; the model checked by the daemon
//! (`switchModel` with no session); then a background client of the daemon,
//! connected in that directory and answering approvals as a bridge's does,
//! sends the prompt and waits for the turn's `turn.started`, whose session
//! id is the answer. Automatic sync takes the session up at that first
//! turn, and the client stays until the session's bridge answers its
//! approvals (or `HOLD` has passed, or the turn is over): a request asked
//! with no answering client following is denied at once. Then it goes, and
//! the turn runs on in the daemon.
//!
//! **Take.** The directory checked as for a start, then `gruntas sync take
//! <session>` run there (`Options::take`), detached, as the person would run
//! it on this machine: this host is still the one that asks for the session,
//! and the take's own checks — the instance by name and ready, a backend's
//! repository trusted, the clone having the commit — refuse before anything
//! moves. Its output is read until it says the session moved here
//! (`moved_line`), when the answer is `taken` and the take is left running,
//! hosting the session as it does on a terminal; or until it ends, when its
//! error's fix, the refusal, is the answer. One at a time: another is
//! `busy`. A take still running after `TAKE_WAIT` is answered
//! `take_unfinished` and left to finish: it may hold the lease by then.
//!
//! **Run** (R-ALLOC-3, R-ALLOC-5): a task a supervisor hands this machine.
//! A job named in no shape a stored job has is refused `bad_job` as the
//! command opens (`host_command::open_command`). Then it is admitted
//! (`admit`) before anything else, in this order: within its
//! `notAfterMs` by this machine's clock (else `bad_job`), accepting (else
//! `not_accepting`), a trusted directory whose origin is the task's (else
//! `no_clone`), the model's instance ready here (else `no_model`), then an
//! agent slot claimed (else `no_slot`), held until the child's turn
//! completes. Then the job is read (never spent: a fallback machine may
//! read it too) and applied into a fresh worktree (`handoff::apply_job`;
//! else `bad_job`, the slot let go), and the child, a full session, runs
//! the one turn, the job's prompt. Its mode is the host's own for that
//! directory, made `default` when it asks nobody, as for a start, then
//! the stricter of that and the `run`'s (`host_command::stricter`): a
//! `run` only ever narrows it (D13). Its lease refused over the machine limit is
//! `machine_limit`. `started` carries the child's session id, and is not
//! sent past `notAfterMs`: the child is stopped and its worktree removed
//! instead, as the supervisor has moved on. When the turn completes, the
//! child's bridge stores its result and logs `task.result` (slice 06),
//! reading the child's `task.json` (`TaskRun`) for what to pack it against.
//! The child's turn runs in the daemon; this host follows it, holding the
//! slot, and denies each approval it asks, keeping them in `task.json`.

use super::announce::{Announcer, Options, FRESH_EVERY};
use super::auto::Config;
use crate::daemon::client::Client as Daemon;
use super::handoff;
use crate::protocol::{ApprovalDecision, Command, LiveEvent, LogBody, PermissionMode, StreamLine};
use serde::{Deserialize, Serialize};
use gruntas_api::sync::DeliveredCommand;
use gruntas_client::e2e::DeviceId;
use gruntas_client::host_command::{self, HostAnswer, HostCommand, RunRefusal, RunTask};
use gruntas_client::presence::{AnnouncedInstance, AnnouncedRepository};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many answered command ids are kept, as the bridge keeps its own.
pub const KEEP: usize = 4096;

/// How many ids that did not open are kept, apart from the answered ones.
const UNOPENED: usize = 256;

/// How long a started session's client stays for its bridge to come and
/// answer the turn's approvals.
const HOLD: Duration = Duration::from_secs(60);

/// How often a held client asks the daemon whether the bridge is there.
const HOLD_EVERY: Duration = Duration::from_millis(200);

/// How long a take is waited for: its own wait for the session's host to
/// hand it over (five minutes), and a minute for the rest.
pub const TAKE_WAIT: Duration = Duration::from_secs(6 * 60);

/// How often a take's output is read.
const TAKE_EVERY: Duration = Duration::from_millis(200);

/// The start of the line `gruntas sync take` says once the session moved
/// here, and before it hosts it: how the take's watcher knows.
pub fn moved_line(session: &str) -> String {
    format!("gruntas: session {session} moved here")
}

/// What is kept of a command answered.
enum Kept {
    /// Answered, not yet sealed: the keys were not to hand.
    Answer { from: DeviceId, answer: HostAnswer },
    /// The sealed answer given, sent again as it is whenever the command is
    /// delivered again: the registry takes the same bytes as a repeat.
    Sealed(Vec<u8>),
}

/// The commands this host has taken, across its publisher's runs.
#[derive(Default)]
pub(crate) struct Commands(Mutex<State>);

#[derive(Default)]
struct State {
    /// Ids answered, oldest first, at most `KEEP`.
    order: VecDeque<String>,
    kept: HashMap<String, Kept>,
    /// Ids that did not open, and when they were last tried.
    unopened: HashMap<String, Instant>,
    /// Ids a thread is working on, or sending the answer to, now.
    working: HashSet<String>,
    /// A take runs.
    taking: bool,
}

impl State {
    fn keep(&mut self, id: &str, k: Kept) {
        self.unopened.remove(id);
        if self.kept.insert(id.to_string(), k).is_none() {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > KEEP {
            if let Some(old) = self.order.pop_front() {
                self.kept.remove(&old);
            }
        }
    }

    fn not_opened(&mut self, id: &str) {
        if self.unopened.len() >= UNOPENED
            && let Some(oldest) = self.unopened.iter().min_by_key(|(_, at)| **at).map(|(id, _)| id.clone())
        {
            self.unopened.remove(&oldest);
        }
        self.unopened.insert(id.to_string(), Instant::now());
    }

    /// The answer to `id` sealed by `a`, if it is not yet, and kept so.
    fn sealed(&mut self, id: &str, a: &Announcer) -> Option<Vec<u8>> {
        let sealed = match self.kept.get(id)? {
            Kept::Sealed(b) => return Some(b.clone()),
            Kept::Answer { from, answer } => match host_command::seal_answer(answer, id, &a.device, from, &a.keys, a.chain.generation()) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("gruntas: the answer to command {id} could not be sealed: {e}; it is tried again at the next refresh");
                    return None;
                }
            },
        };
        self.keep(id, Kept::Sealed(sealed.clone()));
        Some(sealed)
    }
}

impl Commands {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Each delivered command taken: answered again, skipped, refused, or
    /// started on a thread of its own. `host` is this device's name on the
    /// list, named in a refusal. Nothing here waits on the network: an
    /// answer goes from a thread of its own.
    pub(crate) fn take(me: &Arc<Commands>, c: &Arc<Config>, a: &Announcer, host: &str, delivered: Vec<DeliveredCommand>) {
        for d in delivered {
            // The registry's, so said escaped.
            let id = d.id.to_ascii_lowercase();
            let shown = id.escape_debug().to_string();
            {
                let mut s = me.lock();
                if s.working.contains(&id) || s.unopened.get(&id).is_some_and(|at| at.elapsed() < FRESH_EVERY) {
                    continue;
                }
                if s.kept.contains_key(&id) {
                    // Not landed before: sent again as it was.
                    if let Some(sealed) = s.sealed(&id, a) {
                        drop(s);
                        send(me, &id, sealed, a.api.clone());
                    }
                    continue;
                }
            }
            let opened = DeviceId::parse(&d.from).ok_or_else(|| format!("the registry names its sender {:?}, which is no device id", d.from)).and_then(|from| host_command::open_command(&d.sealed, &id, &from, &a.device, &a.keys, &a.chain).map(|o| (from, o)));
            match opened {
                Err(e) => {
                    let mut s = me.lock();
                    if !s.unopened.contains_key(&id) {
                        eprintln!("gruntas: a command delivered to this machine ({shown}) did not open, and is not answered: {e}");
                    }
                    s.not_opened(&id);
                }
                Ok((from, Err(answer))) => {
                    let sealed = {
                        let mut s = me.lock();
                        s.keep(&id, Kept::Answer { from, answer });
                        s.sealed(&id, a)
                    };
                    if let Some(sealed) = sealed {
                        send(me, &id, sealed, a.api.clone());
                    }
                }
                Ok((from, Ok(command))) => {
                    me.lock().working.insert(id.clone());
                    let (me2, c2, id2, host) = (me.clone(), c.clone(), id.clone(), host.to_string());
                    let spawned = std::thread::Builder::new().name("gruntas-host-command".into()).spawn(move || {
                        let told = std::cell::Cell::new(false);
                        let tell = |answer: HostAnswer| {
                            told.set(true);
                            finish(&me2, &c2, &id2, from, answer);
                        };
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&me2, &c2, &host, command, &tell)));
                        if ran.is_err() && !told.get() {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tell(HostAnswer::refused("host_failed", "the host's command panicked — see host.log on it, and send it again"))));
                        }
                        // However it ended, the id is no longer worked on.
                        me2.lock().working.remove(&id2);
                    });
                    if spawned.is_err() {
                        // Not started: delivered again, it is tried again.
                        me.lock().working.remove(&id);
                    }
                }
            }
        }
    }
}

/// A worker's answer kept, then sealed and sent with keys of its own; one
/// that cannot be is sent from the next refresh.
fn finish(me: &Arc<Commands>, c: &Config, id: &str, from: DeviceId, answer: HostAnswer) {
    match &answer {
        HostAnswer::Started { session_id } => eprintln!("gruntas: device {from} started session {session_id} on this machine"),
        HostAnswer::Taken { session_id } => eprintln!("gruntas: device {from} moved session {session_id} to this machine"),
        HostAnswer::Refused { code, message } => eprintln!("gruntas: device {from} asked something of this machine, refused: {} ({})", message.escape_debug(), code.escape_debug()),
    }
    let a = c.announce.as_ref().and_then(|o| (o.keys)(false).ok());
    let sealed = {
        let mut s = me.lock();
        s.keep(id, Kept::Answer { from, answer });
        a.as_ref().and_then(|a| s.sealed(id, a))
    };
    // On a thread of its own already: sent from here, the id still worked on.
    if let (Some(sealed), Some(a)) = (sealed, a) {
        let _ = a.api.answer_host_command(id, &sealed);
    }
    me.lock().working.remove(id);
}

/// `sealed`, the answer to `id`, sent from a thread of its own, the id
/// skipped meanwhile. Not landed, it is sent again at the next delivery.
fn send(me: &Arc<Commands>, id: &str, sealed: Vec<u8>, api: Arc<gruntas_api::Client>) {
    me.lock().working.insert(id.to_string());
    let (me2, id2) = (me.clone(), id.to_string());
    let spawned = std::thread::Builder::new().name("gruntas-host-answer".into()).spawn(move || {
        let _ = api.answer_host_command(&id2, &sealed);
        me2.lock().working.remove(&id2);
    });
    if spawned.is_err() {
        me.lock().working.remove(id);
    }
}

/// The directories this host offers now: the trusted repositories, less
/// those the policy opts out.
pub(crate) fn offered(c: &Config, o: &Options, policy: &super::auto::Policy) -> Vec<String> {
    let worktrees = gruntas_api::home::worktrees_root(&gruntas_api::home::process_env);
    o.trusted.directories().into_iter().filter(|d| not_krowks_own(d, worktrees.as_deref()) && policy.opted_out(d, c.krowk_home.as_deref()).is_none()).map(|d| d.display().to_string()).collect()
}

/// The directories as repositories: each one's `origin` remote, normalised
/// as it is announced (`presence::normalise_origin`); one with none, or a
/// local path, is left out.
fn repositories(directories: &[String]) -> Vec<AnnouncedRepository> {
    directories
        .iter()
        .filter_map(|d| {
            let out = gruntas_api::git::command(Path::new(d)).ok()?.args(["config", "--get", "remote.origin.url"]).output().ok().filter(|o| o.status.success())?;
            let origin = gruntas_client::presence::normalise_origin(String::from_utf8_lossy(&out.stdout).trim())?;
            Some(AnnouncedRepository { directory: d.clone(), origin })
        })
        .collect()
}

/// Whether `dir` is a repository rather than a worktree gruntas made under
/// `worktrees`, which a handoff or a task trusts for its backend but is
/// no clone to offer.
fn not_krowks_own(dir: &Path, worktrees: Option<&Path>) -> bool {
    worktrees.is_none_or(|w| !dir.starts_with(w))
}

/// A command, run: a start's answer told to `tell` once known, then held
/// for the bridge (`hold`); a take's once it moved or was refused.
fn run(me: &Arc<Commands>, c: &Config, host: &str, command: HostCommand, tell: &dyn Fn(HostAnswer)) {
    let Some(o) = c.announce.as_ref() else { return tell(HostAnswer::refused("host_failed", "this host takes no commands")) };
    let cwd = match command {
        HostCommand::Run(task) => return run_task(c, o, host, task, tell),
        HostCommand::Start { ref cwd, .. } | HostCommand::Take { ref cwd, .. } => cwd.clone(),
    };
    let policy = match (c.policy)() {
        Ok(p) if p.on => p,
        Ok(_) => return tell(HostAnswer::refused("sync_off", format!("automatic sync is off on {host} — turn it on there to start or move sessions from your other devices"))),
        Err(e) => return tell(HostAnswer::refused("sync_off", format!("{host} cannot read its sync settings: {e}"))),
    };
    let offered = offered(c, o, &policy);
    if !offered.contains(&cwd) {
        let pick = if offered.is_empty() { "it offers none yet".to_string() } else { format!("pick one of: {}", offered.join(", ")) };
        return tell(HostAnswer::refused("directory_not_offered", format!("{host} does not offer {cwd}: run `gruntas trust {}` on {host}, or {pick}", crate::worktree::manage::quote(Path::new(&cwd)))));
    }
    let (model, prompt) = match command {
        HostCommand::Start { model, prompt, .. } => (model, prompt),
        HostCommand::Take { session_id, .. } if crate::log::valid_id(&session_id) => return take(me, c, o, host, &session_id, &cwd, tell),
        HostCommand::Take { session_id, .. } => return tell(HostAnswer::refused("bad_command", format!("{:?} is not a session id", session_id.escape_debug().to_string()))),
        HostCommand::Run(_) => unreachable!("run is answered above"),
    };
    // The host's own default for the directory, never what the command
    // carried, and never a mode that asks nobody.
    let mode = match (o.mode_for)(Path::new(&cwd)) {
        Ok(m) if m.asks_nothing() => PermissionMode::Default,
        Ok(m) => m,
        Err((code, message)) => return tell(HostAnswer::refused(&code, message)),
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return tell(HostAnswer::refused("host_failed", format!("{host} has no async runtime for the start — try again")));
    };
    rt.block_on(start(c, &cwd, model, prompt, mode, tell));
}

/// What a host holds a `run` to before it answers (R-ALLOC-5): what it
/// would announce of itself, read now, and its agent slots. Its
/// repositories and instances are read directly (as `announce::fill`
/// builds them), never from the published record, which `announce::fit`
/// may have trimmed: a repository left out of the record is still here.
pub struct Admission<'a> {
    /// This machine's clock, in milliseconds since the Unix epoch.
    pub now_ms: i64,
    pub accepting: bool,
    pub repositories: &'a [AnnouncedRepository],
    pub instances: &'a [AnnouncedInstance],
    pub agents: &'a crate::slots::Pool,
}

/// A `run` admitted: the trusted directory it runs in, the model it runs
/// on, and the agent slot it holds until its turn completes.
#[derive(Debug)]
pub struct Admitted {
    pub directory: std::path::PathBuf,
    pub model: crate::protocol::ModelRef,
    pub slot: crate::slots::Slot,
}

/// `task` checked against `here` in the module's order: the first check it
/// fails is the refusal, with its fix line; passing them all, the agent
/// slot is held in the `Admitted` (D9).
pub fn admit(task: &RunTask, here: &Admission<'_>) -> Result<Admitted, (RunRefusal, String)> {
    if here.now_ms > task.not_after_ms {
        return Err((RunRefusal::BadJob, "the task came after the time its sender gave up waiting for an answer, so it was not run — send it again".into()));
    }
    if !here.accepting {
        return Err((RunRefusal::NotAccepting, "this machine does not take tasks from your other machines — gruntas machines accept on, there, turns that on".into()));
    }
    let Some(repository) = here.repositories.iter().find(|r| r.origin == task.origin) else {
        return Err((RunRefusal::NoClone, format!("this machine has no trusted clone of {}", task.origin)));
    };
    match here.instances.iter().find(|i| i.name == task.model.instance) {
        Some(i) if i.ready => {}
        Some(i) => return Err((RunRefusal::NoModel, i.fix.clone().unwrap_or_else(|| format!("the {} account is not ready on this machine ({})", i.name, i.state)))),
        None => return Err((RunRefusal::NoModel, format!("this machine has no {} account", task.model.instance))),
    }
    match here.agents.try_take() {
        Ok(Some(slot)) => Ok(Admitted { directory: std::path::PathBuf::from(&repository.directory), model: task.model.clone(), slot }),
        Ok(None) => Err((RunRefusal::NoSlot, format!("all {} of this machine's agent slots are in use", here.agents.size()))),
        Err(e) => Err((RunRefusal::NoSlot, format!("this machine's agent slots could not be read: {e}"))),
    }
}

/// The file a task's child leaves in its session's directory for its bridge
/// (slice 06): where the job's state was recorded and the worktree the child
/// works in, so the bridge packs the result against it when the turn ends,
/// and the approvals denied so far, which it logs with the result (R-ALLOC-14).
pub const TASK_FILE: &str = "task.json";

/// The contents of `TASK_FILE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRun {
    /// The snapshot commit of the job's state in the worktree
    /// (`handoff::JobApplied::snapshot`): what the result is packed against.
    pub snapshot: String,
    /// The worktree the child works in.
    pub worktree: String,
    /// What the child asked and was refused, one line each, in order.
    #[serde(default)]
    pub denied: Vec<String>,
}

/// The task record in `sessions/<session>`, if the session is a task's child.
pub fn read_task(sessions: &Path, session: &str) -> Option<TaskRun> {
    serde_json::from_slice(&std::fs::read(sessions.join(session).join(TASK_FILE)).ok()?).ok()
}

/// `t` written in `sessions/<session>`, all at once.
fn write_task(sessions: &Path, session: &str, t: &TaskRun) -> Result<(), String> {
    let dir = sessions.join(session);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} could not be made: {e}", dir.display()))?;
    let (tmp, to) = (dir.join(format!("{TASK_FILE}.tmp")), dir.join(TASK_FILE));
    let bytes = serde_json::to_vec(t).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &to)).map_err(|e| format!("{} could not be written: {e}", to.display()))
}

/// How long a child waits to learn its lease: its bridge takes it up at the
/// child's first turn, and a supervisor gives up on the answer in 30 seconds.
const LEASE_WAIT: Duration = Duration::from_secs(20);

/// How many denied approvals a task's record keeps, and how long each line is.
const MAX_DENIED: usize = 100;
const DENIED_CHARS: usize = 200;

/// How often the registry is asked whether the child's lease is this
/// machine's.
const LEASE_EVERY: Duration = Duration::from_millis(200);

/// The permission mode a handed-over task's child runs in (D13): the
/// host's own for the directory, as a remote start runs in it (never one
/// that asks nobody), or the supervisor's when that is stricter.
fn child_mode(host: PermissionMode, asked: Option<PermissionMode>) -> PermissionMode {
    let host = if host.asks_nothing() { PermissionMode::Default } else { host };
    asked.map_or(host, |a| host_command::stricter(host, a))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The job read from the registry and opened (never spent: a fallback
/// machine may read it too).
fn read_job(api: &gruntas_api::Client, job: &host_command::JobRef) -> Result<handoff::Bundle, String> {
    let key = handoff::key_from_hex(&job.key).ok_or("the job's key is not 64 hex digits")?;
    let sealed = api.read_transport(&job.transport).map_err(|e| format!("the job could not be read from the registry: {}", e.fix()))?;
    let plain = gruntas_client::e2e::open_transport(&sealed, &crate::daemon::ws::uuid(&job.session), &key).map_err(|e| e.to_string())?;
    handoff::Bundle::decode(&plain, &job.session)
}

/// A `run`, answered (R-ALLOC-3, R-ALLOC-5): admitted, the job read and
/// applied in a fresh worktree of the trusted clone, then the child — a
/// full session in that worktree, the one turn the job's prompt — started
/// and followed until the turn is over, holding the agent slot throughout.
/// `started` is told once the child's bridge holds its lease, so one
/// refused over the machine limit is `machine_limit` instead, and never
/// past `notAfterMs`: the child is then stopped and its worktree removed.
fn run_task(c: &Config, o: &Options, host: &str, task: RunTask, tell: &dyn Fn(HostAnswer)) {
    // Read now, never from the announcement, which is trimmed to fit.
    let (accepting, repositories) = match (c.policy)() {
        Ok(p) => (p.on && p.accepting, repositories(&offered(c, o, &p))),
        Err(e) => return tell(RunRefusal::NotAccepting.answer(format!("{host} cannot read its sync settings: {e}"))),
    };
    let (registry, reports) = (o.instances)();
    let Some(agents) = registry.agents.as_ref() else { return tell(RunRefusal::NoSlot.answer(format!("{host} has no agent slots to run a task in"))) };
    let instances = super::announce::instances(&registry, &reports, None);
    let here = Admission { now_ms: now_ms(), accepting, repositories: &repositories, instances: &instances, agents };
    let Admitted { directory, model, slot } = match admit(&task, &here) {
        Ok(a) => a,
        Err((refusal, message)) => return tell(refusal.answer(message)),
    };
    let bad = |message: String| tell(RunRefusal::BadJob.answer(message));
    let a = match (o.keys)(false) {
        Ok(a) => a,
        Err((code, message)) => return tell(HostAnswer::refused(&code, message)),
    };
    let host_mode = match (o.mode_for)(&directory) {
        Ok(m) => m,
        Err((code, message)) => return tell(HostAnswer::refused(&code, message)),
    };
    let mode = child_mode(host_mode, task.permission_mode);
    let bundle = match read_job(&a.api, &task.job) {
        Ok(b) if b.prompt.trim().is_empty() => return bad("the job carries no prompt".into()),
        Ok(b) => b,
        Err(e) => return bad(e),
    };
    let Some(worktrees) = gruntas_api::home::worktrees_root(&gruntas_api::home::process_env) else { return tell(HostAnswer::refused("host_failed", format!("{host} found no home directory to make worktrees under"))) };
    let handoff::JobApplied { applied: handoff::Applied { worktree, held, cwd }, snapshot } = match handoff::apply_job(&bundle, &directory, &worktrees) {
        Ok(j) => j,
        Err(e) => return bad(e),
    };
    // The worktree is the trusted repository's, which a backend may run in.
    let _ = o.trusted.trust(&crate::trust::root(&cwd));
    let child = Child { cwd, worktree: worktree.path.display().to_string(), snapshot, model, mode, prompt: bundle.prompt };
    // Removed however this ends, a panic included, unless the child ran.
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut fresh = Fresh(Some(worktree), ran.clone());
    let kept = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(follow(c, &a, &task, &child, &ran, tell)),
        Err(_) => {
            tell(HostAnswer::refused("host_failed", format!("{host} has no async runtime for the task — try again")));
            false
        }
    };
    if kept {
        // The turn is over: the worktree stays for the bridge to pack, as a
        // finished child's does, and the slot is let go.
        fresh.0.take();
    } else {
        // Over, stopped or never started: nothing runs in it.
        ran.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    drop((fresh, held, slot));
}

/// A worktree made for a task, removed when dropped, unless its child's
/// turn was started (the flag): one that may still be running keeps it.
struct Fresh(Option<crate::worktree::Worktree>, Arc<std::sync::atomic::AtomicBool>);

impl Drop for Fresh {
    fn drop(&mut self) {
        if let Some(w) = self.0.take().filter(|_| !self.1.load(std::sync::atomic::Ordering::SeqCst)) {
            let _ = crate::worktree::discard(&w);
        }
    }
}

/// What a child runs with.
struct Child {
    cwd: std::path::PathBuf,
    worktree: String,
    snapshot: String,
    model: crate::protocol::ModelRef,
    mode: PermissionMode,
    prompt: String,
}

/// Where a started child stands on its way to `started`.
struct Following<'a> {
    c: &'a Config,
    a: &'a Announcer,
    task: &'a RunTask,
    child: &'a Child,
    session: Option<String>,
    denied: Vec<String>,
    /// Told when the turn started: the lease asked for, at the registry.
    lease: Option<std::sync::mpsc::Receiver<Result<(), HostAnswer>>>,
    told: bool,
    /// The child must not go on: the answer owed, none for one past its deadline.
    stopping: Option<Option<HostAnswer>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Following<'_> {
    fn record(&self) -> Result<(), String> {
        let Some(s) = &self.session else { return Ok(()) };
        write_task(&self.c.sessions, s, &TaskRun { snapshot: self.child.snapshot.clone(), worktree: self.child.worktree.clone(), denied: self.denied.clone() })
    }

    async fn stop_child(&mut self, d: &Daemon, answer: Option<HostAnswer>) {
        if self.stopping.is_some() {
            return;
        }
        self.stopping = Some(answer);
        if let Some(s) = &self.session {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let _ = d.execute(Command::Interrupt { session_id: s.clone() }, tx).await;
        }
    }

    /// One line of the child's stream: its first turn starting is where
    /// its record is written and its lease watched for; an approval it asks
    /// is denied, and kept (D13, R-ALLOC-14).
    async fn line(&mut self, d: &Daemon, l: StreamLine) {
        match l {
            StreamLine::Log(ev) if self.session.is_none() && matches!(ev.body, LogBody::TurnStarted { .. }) => {
                self.session = Some(ev.session_id.clone());
                if let Err(e) = self.record() {
                    return self.stop_child(d, Some(HostAnswer::refused("host_failed", e))).await;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.lease = Some(rx);
                watch_lease(self.a.api.clone(), ev.session_id, self.a.device.to_string(), self.stop.clone(), tx);
            }
            StreamLine::Live(LiveEvent::ApprovalRequested(r)) => {
                deny(&mut self.denied, &r.summary);
                // Kept before it is answered, so the result never misses it.
                let _ = self.record();
                let (tx, _rx) = tokio::sync::mpsc::channel(1);
                let _ = d.execute(Command::Approve { session_id: r.session_id, request_id: r.request_id, decision: ApprovalDecision::Deny, answers: Vec::new() }, tx).await;
            }
            _ => {}
        }
    }

    /// The lease's outcome, once it is known: `started` told, or the child
    /// stopped.
    async fn lease_known(&mut self, d: &Daemon, tell: &dyn Fn(HostAnswer)) {
        if self.told || self.stopping.is_some() {
            return;
        }
        let Some(rx) = &self.lease else { return };
        let known = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(HostAnswer::refused("host_failed", "the child's lease could not be watched")),
        };
        match known {
            // Past it, by this clock: the supervisor has moved on.
            Ok(()) if now_ms() > self.task.not_after_ms => self.stop_child(d, None).await,
            Ok(()) => {
                if let Some(session_id) = self.session.clone() {
                    // Told, and so sent, while this client still answers
                    // the turn's approvals.
                    tell(HostAnswer::Started { session_id });
                    self.told = true;
                }
            }
            Err(answer) => self.stop_child(d, Some(answer)).await,
        }
    }
}

/// `summary` of a refused approval added to `denied`: one short line, and
/// at most `MAX_DENIED` of them.
fn deny(denied: &mut Vec<String>, summary: &str) {
    match denied.len().cmp(&MAX_DENIED) {
        std::cmp::Ordering::Less => {
            let line = summary.lines().next().unwrap_or_default();
            let cut = line.char_indices().nth(DENIED_CHARS).map_or(line.len(), |(i, _)| i);
            denied.push(if cut < line.len() { format!("{}…", &line[..cut]) } else { line.to_string() });
        }
        std::cmp::Ordering::Equal => denied.push("and more it asked after these".into()),
        std::cmp::Ordering::Greater => {}
    }
}

/// Stops a child's watch on its lease once dropped.
struct Watch(Arc<std::sync::atomic::AtomicBool>);

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Asks the registry until `session`'s lease is `me`'s, or the bridge was
/// refused over the machine limit, or `LEASE_WAIT` has passed, and says
/// which.
fn watch_lease(api: Arc<gruntas_api::Client>, session: String, me: String, stop: Arc<std::sync::atomic::AtomicBool>, tx: std::sync::mpsc::Sender<Result<(), HostAnswer>>) {
    let _ = std::thread::Builder::new().name("gruntas-task-lease".into()).spawn(move || {
        let until = Instant::now() + LEASE_WAIT;
        let said = loop {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let Some(said) = super::auto::limited(&session) {
                break Err(RunRefusal::MachineLimit.answer(said));
            }
            if api.show_sync_session(&session).ok().and_then(|s| s.lease).is_some_and(|l| l.device.eq_ignore_ascii_case(&me)) {
                break Ok(());
            }
            if Instant::now() >= until {
                break Err(HostAnswer::refused("not_synced", "the child's session did not sync here in time, so it could not be followed — is automatic sync on?"));
            }
            std::thread::sleep(LEASE_EVERY);
        };
        let _ = tx.send(said);
    });
}

/// The child run and followed to the end of its turn (R-ALLOC-5): its
/// answer told as `lease_known` says, its approvals denied. Answers whether
/// its worktree is kept: a child stopped before it was told started, or
/// that never started, leaves nothing.
async fn follow(c: &Config, a: &Announcer, task: &RunTask, child: &Child, ran: &std::sync::atomic::AtomicBool, tell: &dyn Fn(HostAnswer)) -> bool {
    let refused = |e: crate::engine::EngineError| HostAnswer::refused(&e.code, e.message);
    // Background, answering approvals as a bridge's client does, so none
    // the turn asks waits for a person: it is denied here.
    let d = match Daemon::connect_as(&c.socket, &child.cwd, &c.version, true, true).await {
        Ok(d) => d,
        Err(e) => {
            tell(match e {
                crate::daemon::client::ConnectError::Failed(e) => refused(e),
                crate::daemon::client::ConnectError::Absent => HostAnswer::refused("host_unavailable", "the host daemon is not running — it starts again with the next gruntas there"),
            });
            return false;
        }
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    if let Err(e) = d.execute(Command::SwitchModel { session_id: None, model: child.model.clone() }, tx).await {
        tell(refused(e));
        return false;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let budget = (!task.budget.is_empty()).then_some(task.budget);
    let prompt = Command::Prompt { session_id: None, text: child.prompt.clone(), images: Vec::new(), model: Some(child.model.clone()), permission_mode: child.mode, toolset: None, effort: None, rollover: None, budget };
    ran.store(true, std::sync::atomic::Ordering::SeqCst);
    let run = d.execute(prompt, tx);
    tokio::pin!(run);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _watch = Watch(stop.clone());
    let mut f = Following { c, a, task, child, session: None, denied: Vec::new(), lease: None, told: false, stopping: None, stop };
    let (mut ended, mut closed, mut failed) = (false, false, None);
    loop {
        let tick = tokio::time::sleep(LEASE_EVERY);
        tokio::pin!(tick);
        loop {
            tokio::select! {
                // The tick first, so a stream that never pauses does not hold
                // `started` back; then the turn's lines, which all arrive
                // before its end.
                biased;
                () = &mut tick => break,
                line = rx.recv(), if !closed => match line {
                    Some(l) => f.line(&d, l).await,
                    None => closed = true,
                },
                done = &mut run, if !ended => {
                    ended = true;
                    while let Ok(l) = rx.try_recv() {
                        f.line(&d, l).await;
                    }
                    if let Err(e) = done {
                        failed = Some(e);
                    }
                }
            }
        }
        f.lease_known(&d, tell).await;
        // Over once the turn ended and its answer is settled: the slot is
        // held until the turn is, a stopped child's included.
        if ended && (f.told || f.stopping.is_some() || f.session.is_none()) {
            break;
        }
    }
    match f.stopping {
        Some(answer) => {
            // Never a refusal after `started`.
            if let (Some(answer), false) = (answer, f.told) {
                tell(answer);
            }
            false
        }
        None if f.told => true,
        // The prompt ended before a turn started.
        None => {
            tell(match failed {
                Some(e) => refused(e),
                None => HostAnswer::refused("not_started", "the task's prompt ended before a turn started — send it again"),
            });
            false
        }
    }
}

async fn start(c: &Config, cwd: &str, model: crate::protocol::ModelRef, prompt: String, mode: PermissionMode, tell: &dyn Fn(HostAnswer)) {
    let refused = |e: crate::engine::EngineError| HostAnswer::refused(&e.code, e.message);
    // Background, so it never keeps the daemon up; answering approvals, as
    // a bridge's client does, so the turn's are asked of its followers.
    let d = match Daemon::connect_as(&c.socket, Path::new(cwd), &c.version, true, true).await {
        Ok(d) => d,
        Err(crate::daemon::client::ConnectError::Failed(e)) => return tell(refused(e)),
        Err(crate::daemon::client::ConnectError::Absent) => return tell(HostAnswer::refused("host_unavailable", "the host daemon is not running — it starts again with the next gruntas there")),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    if let Err(e) = d.execute(Command::SwitchModel { session_id: None, model: model.clone() }, tx).await {
        return tell(refused(e));
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let prompt = Command::Prompt { session_id: None, text: prompt, images: Vec::new(), model: Some(model), permission_mode: mode, toolset: None, effort: None, rollover: None, budget: None };
    let run = d.execute(prompt, tx);
    tokio::pin!(run);
    let started = |l: &StreamLine| match l {
        StreamLine::Log(ev) if matches!(ev.body, LogBody::TurnStarted { .. }) => Some(ev.session_id.clone()),
        _ => None,
    };
    // No deadline of its own: a refusal said while a turn may yet start
    // would have the sender start it again. A daemon that goes ends it.
    let mut closed = false;
    let session = loop {
        tokio::select! {
            // The turn's lines first: they all arrive before its end. Their
            // stream closes as the command is done, a refusal included, so
            // the end is waited for then.
            biased;
            line = rx.recv(), if !closed => match line {
                Some(l) => if let Some(s) = started(&l) { break s },
                None => closed = true,
            },
            done = &mut run => {
                if let Some(s) = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|l| started(&l)) {
                    return tell(HostAnswer::Started { session_id: s });
                }
                return tell(match done {
                    Err(e) => refused(e),
                    Ok(_) => HostAnswer::refused("not_started", "the prompt ended before a turn started — send it again"),
                });
            }
        }
    };
    // Told, and so sent, while this client still answers the turn's
    // approvals: sending blocks this thread's runtime, and the daemon
    // queues this client's lines meanwhile.
    tell(HostAnswer::Started { session_id: session.clone() });
    hold(&d, &session, &mut rx, run).await;
}

/// A take, run (R-HAND-1, R-HAND-4): `gruntas sync take` started in `cwd`
/// and watched until it says the session moved here, or ends refused. A
/// client of the daemon is held meanwhile, so it does not exit idle with
/// the answer unsent. Past `TAKE_WAIT` the take is left to finish, never
/// stopped: it may hold the session's lease by then.
fn take(me: &Arc<Commands>, c: &Config, o: &Options, host: &str, session: &str, cwd: &str, tell: &dyn Fn(HostAnswer)) {
    let Some(up) = &o.take else { return tell(HostAnswer::refused("take_unavailable", format!("{host} doesn't take sessions up from your other devices — upgrade gruntas there"))) };
    if std::mem::replace(&mut me.lock().taking, true) {
        return tell(HostAnswer::refused("busy", format!("{host} is taking another session up — try again once it has")));
    }
    // Cleared however this ends, unless handed to the thread that waits
    // for a take still running.
    struct Taking(Option<Arc<Commands>>);
    impl Drop for Taking {
        fn drop(&mut self) {
            if let Some(me) = self.0.take() {
                me.lock().taking = false;
            }
        }
    }
    let mut taking = Taking(Some(me.clone()));
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return tell(HostAnswer::refused("host_failed", format!("{host} has no async runtime for the take — try again")));
    };
    let (mut child, log) = match up(session, Path::new(cwd)) {
        Ok(started) => started,
        Err(e) => return tell(HostAnswer::refused("host_failed", format!("{host} couldn't run gruntas sync take: {e}"))),
    };
    let moved = moved_line(session);
    let read = {
        let log = log.clone();
        move || std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    };
    let has_moved = {
        let moved = moved.clone();
        move |out: &str| out.lines().any(|l| l.starts_with(&moved))
    };
    let (answer, held) = rt.block_on(async {
        // Not a background client: the daemon stays up until the answer is
        // sent.
        let held = Daemon::connect(&c.socket, Path::new(cwd), &c.version, false).await.ok();
        let taken = || HostAnswer::Taken { session_id: session.to_string() };
        let until = Instant::now() + TAKE_WAIT;
        let answer = loop {
            if has_moved(&read()) {
                break Some(taken());
            }
            if let Ok(Some(status)) = child.try_wait() {
                let out = read();
                break Some(match refusal(&out) {
                    _ if has_moved(&out) => taken(),
                    // The same take asked again (a daemon restarted under
                    // it): it moved here the first time.
                    Some((code, _)) if code == "already_here" => taken(),
                    Some((_, fix)) => HostAnswer::refused("take_refused", fix),
                    None => HostAnswer::refused("take_refused", format!("gruntas sync take on {host} ended ({status}) and said nothing — the session stays on its host")),
                });
            }
            if Instant::now() >= until {
                break None;
            }
            tokio::time::sleep(TAKE_EVERY).await;
        };
        (answer, held)
    });
    let answer = answer.unwrap_or_else(|| {
        HostAnswer::refused("take_unfinished", format!("gruntas sync take on {host} hasn't finished after {} minutes; it goes on, and the session may yet move — see {} there", TAKE_WAIT.as_secs() / 60, log.display()))
    });
    let unfinished = matches!(answer, HostAnswer::Refused { ref code, .. } if code == "take_unfinished");
    if !matches!(answer, HostAnswer::Refused { ref code, .. } if code == "take_refused") {
        // Hosting the session now, or still taking it: left to run, and
        // reaped once it ends. One still taking it stays the take under way
        // — another is `busy` — until it moves the session or ends.
        let still = Taking(if unfinished { taking.0.take() } else { None });
        let _ = std::thread::Builder::new().name("gruntas-take".into()).spawn(move || {
            while still.0.is_some() && !has_moved(&read()) && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(TAKE_EVERY);
            }
            drop(still);
            let _ = child.wait();
        });
    }
    drop(taking);
    tell(answer);
    drop(held);
}

/// What a take that ended refused with, as `(code, fix)`: the error it
/// wrote last, as JSON (`{"ok": false, "error": {"error": …, "fix": …}}`,
/// as `--format json` writes one); else its last line, with no code.
fn refusal(out: &str) -> Option<(String, String)> {
    let error = out.match_indices('{').map(|(i, _)| i).rfind(|i| *i == 0 || out[..*i].ends_with('\n')).and_then(|i| {
        let v: serde_json::Value = serde_json::from_str(out[i..].trim()).ok()?;
        let fix = v["error"]["fix"].as_str().or_else(|| v["error"]["message"].as_str())?;
        Some((v["error"]["error"].as_str().unwrap_or_default().to_string(), fix.to_string()))
    });
    error.or_else(|| out.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| (String::new(), l.strip_prefix("gruntas: ").unwrap_or(l).to_string())))
}

/// Stays, following the session, until another client answers its
/// approvals — its bridge — or the turn is over, or `HOLD` has passed.
async fn hold<F: std::future::Future>(d: &Daemon, session: &str, rx: &mut tokio::sync::mpsc::Receiver<StreamLine>, run: std::pin::Pin<&mut F>) {
    let until = Instant::now() + HOLD;
    let mut run = run;
    let (mut ended, mut closed) = (false, false);
    while !ended && Instant::now() < until {
        let tick = tokio::time::sleep(HOLD_EVERY);
        tokio::pin!(tick);
        // Its lines drained meanwhile, so its stream never backs up.
        loop {
            tokio::select! {
                line = rx.recv(), if !closed => closed = line.is_none(),
                _ = &mut run, if !ended => ended = true,
                () = &mut tick => break,
            }
        }
        let Ok(Ok(status)) = tokio::time::timeout(HOLD_EVERY * 5, d.status()).await else { return };
        match status.sessions.iter().find(|s| s.session_id == session) {
            Some(s) if s.running && s.answering < 2 => {}
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_client::host_command::JobRef;
    use gruntas_client::protocol::BudgetLimits;

    /// Slice 05 (R-ALLOC-3, R-ALLOC-5, D9, D14): each refusal in its case,
    /// checked in order, and an admitted run holding an agent slot.
    #[test]
    fn alloc_05_a_run_is_refused_in_each_case_and_admitted_holding_a_slot() {
        let runtime = std::env::temp_dir().join(format!("krowk-admit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        let agents = crate::slots::Pool::new(runtime.clone(), crate::children::AGENT_SLOTS, 1);
        let repositories = [AnnouncedRepository { directory: "/home/ann/app".into(), origin: "github.com/acme/app".into() }];
        let instances = [AnnouncedInstance { name: "anthropic".into(), kind: "anthropic".into(), ready: true, state: "ready".into(), fix: None, models: vec!["claude-opus-5-5".into()], limit: None }];
        let here = Admission { now_ms: 1_000, accepting: true, repositories: &repositories, instances: &instances, agents: &agents };
        let task = RunTask {
            origin: "github.com/acme/app".into(),
            model: crate::protocol::ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() },
            job: JobRef { session: "0190f3c2-0000-7000-8000-000000000001".into(), transport: "art_1".into(), key: "ab".repeat(32) },
            budget: BudgetLimits::default(),
            requested_by: "00".repeat(16),
            permission_mode: None,
            not_after_ms: 31_000,
        };
        let refused = |t: &RunTask, h: &Admission<'_>| admit(t, h).map(|_| ()).unwrap_err().0;
        assert_eq!(refused(&task, &Admission { now_ms: 31_001, ..here }), RunRefusal::BadJob, "past its deadline: the supervisor has moved on");
        assert_eq!(refused(&task, &Admission { accepting: false, ..here }), RunRefusal::NotAccepting);
        assert_eq!(refused(&RunTask { origin: "github.com/acme/other".into(), ..task.clone() }, &here), RunRefusal::NoClone);
        assert_eq!(refused(&RunTask { model: crate::protocol::ModelRef { instance: "codex".into(), model: "gpt-5.5".into() }, ..task.clone() }, &here), RunRefusal::NoModel);
        let admitted = admit(&task, &here).unwrap();
        assert_eq!(admitted.directory, std::path::PathBuf::from("/home/ann/app"));
        assert_eq!(admitted.model.instance, "anthropic");
        assert_eq!(refused(&task, &here), RunRefusal::NoSlot, "its one agent slot is held by the first");
        drop(admitted);
        assert!(admit(&task, &here).is_ok(), "let go as the child's turn completes");
        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// D13: the child runs in the host's own mode, or the supervisor's when
    /// that is stricter, and never one that asks nobody.
    #[test]
    fn a_tasks_child_runs_in_the_hosts_mode_or_a_stricter_one() {
        use PermissionMode::*;
        assert_eq!(child_mode(AcceptEdits, None), AcceptEdits);
        assert_eq!(child_mode(AcceptEdits, Some(Default)), Default, "stricter asked");
        assert_eq!(child_mode(AcceptEdits, Some(Plan)), Plan);
        assert_eq!(child_mode(Default, Some(AcceptEdits)), Default, "never looser");
        assert_eq!(child_mode(Default, Some(Unhinged)), Default);
        assert_eq!(child_mode(Plan, Some(Default)), Plan);
        assert_eq!(child_mode(Unhinged, None), Default, "a host that asks nobody still has a remote child ask");
        assert_eq!(child_mode(BypassPermissions, Some(Plan)), Plan);
    }

    /// The record a child leaves for its bridge reads back as written, and a
    /// session that is no task's has none.
    #[test]
    fn a_tasks_record_is_read_back_by_its_session() {
        let dir = std::env::temp_dir().join(format!("krowk-task-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_task(&dir, "s1"), None);
        let mut t = TaskRun { snapshot: "abc".into(), worktree: "/w/x".into(), denied: Vec::new() };
        write_task(&dir, "s1", &t).unwrap();
        assert_eq!(read_task(&dir, "s1"), Some(t.clone()));
        t.denied.push("Bash `rm -rf build`".into());
        write_task(&dir, "s1", &t).unwrap();
        assert_eq!(read_task(&dir, "s1"), Some(t));
        assert_eq!(read_task(&dir, "s2"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A machine over its limit is told by the supervisor's refusal.
    #[test]
    fn a_session_refused_over_the_machine_limit_is_found() {
        assert_eq!(super::super::auto::limited("limited-session"), None);
        super::super::auto::limit_refused("limited-session", "Free syncs sessions from two machines");
        assert_eq!(super::super::auto::limited("limited-session").as_deref(), Some("Free syncs sessions from two machines"));
    }

    /// A worktree gruntas made, trusted for its backend, is not offered as a
    /// clone; a repository beside it is.
    #[test]
    fn a_krowk_worktree_is_not_offered_as_a_clone() {
        let root = Path::new("/home/ann/.local/share/gruntas/worktrees");
        assert!(!not_krowks_own(&root.join("4fb7/e117ff1a"), Some(root)));
        assert!(not_krowks_own(Path::new("/home/ann/app"), Some(root)));
        assert!(not_krowks_own(Path::new("/home/ann/app"), None));
    }

    /// A child's denials are short lines, and few.
    #[test]
    fn denied_approvals_are_kept_short_and_bounded() {
        let long = format!("Bash `{}`\nsecond line", "x".repeat(500));
        let mut denied = Vec::new();
        deny(&mut denied, &long);
        assert!(denied[0].chars().count() <= DENIED_CHARS + 1 && !denied[0].contains('\n'));
        for _ in 0..MAX_DENIED * 2 {
            deny(&mut denied, "Bash `ls`");
        }
        assert_eq!(denied.len(), MAX_DENIED + 1);
        assert_eq!(denied.last().unwrap(), "and more it asked after these");
    }

    #[test]
    fn r_hand_4_a_takes_refusal_is_its_last_errors_fix() {
        // What `gruntas sync take` writes when its output is no terminal: its
        // lines on stderr, then the error as JSON.
        let out = "gruntas: asking the session's host to hand it over…\n{\n  \"ok\": false,\n  \"error\": {\n    \"error\": \"instance_missing\",\n    \"fix\": \"session s runs on the work-claude instance, which this machine does not have — the session stays where it is\",\n    \"retryable\": false\n  }\n}\n";
        assert_eq!(refusal(out), Some(("instance_missing".into(), "session s runs on the work-claude instance, which this machine does not have — the session stays where it is".into())));
        assert_eq!(refusal("gruntas: something went wrong\n\n"), Some((String::new(), "something went wrong".into())));
        assert_eq!(refusal(""), None);
    }

    #[test]
    fn r_hand_4_the_moved_line_names_the_session() {
        assert!(format!("{}, working in /w (branch b); hosting it until interrupted", moved_line("s1")).starts_with("gruntas: session s1 moved here"));
    }
}
