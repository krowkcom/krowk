//! Placement (R-ALLOC-7 … R-ALLOC-11; engineering/harness.md → Automatic
//! allocation): which of the person's machines runs a task a supervisor
//! hands over, and the task's way there and back. krowk's own `subagent`
//! tool and krowk-mcp's `krowk_delegate` and `krowk_collect` are its two
//! callers, and share all of it.
//!
//! **It runs on the supervisor's machine** (D3), from the announcements
//! opened there (`krowk_client::presence`): the registry sees only sealed
//! bytes, so it cannot place anything.
//!
//! **The candidates.** Each other machine as its announcement says, and
//! this one as it is now: its capacity read directly
//! (`announce::capacity`), never through its own announcement, and its
//! repository the task's own, so neither `accepting` nor a clone applies
//! to it.
//!
//! **The pick** (`choose`, pure). Every machine not `ruled_out` (D6, D7,
//! D14: offline, not accepting, no clone of the repository's origin, no
//! ready instance of the model's, no capacity announced (a host older than
//! allocation), no free agent slot counting this supervisor's own
//! placements on it of the last `RECENT` as slots in use, D9, added
//! saturating; this machine only for the model). Then this machine unless it is
//! `busy` (D4, D5); else the others ranked by free agent slots, then free
//! build slots, then lowest load per core, then most free memory; a
//! machine whose instance of the model reports it is near its usage limit
//! comes after every one whose does not (how this reads D6's "less
//! preferred"). A `HostPin` narrows it: a machine by name, `any` (the most
//! room, this machine no more preferred than another), or `any:<os>`. The
//! picks are the first, at most one more machine to send to (D11), and
//! last this machine, when it can take the task and is not already first:
//! a task whose remote picks all refused runs here, busy or not. A pin to
//! another machine by name has no fallback.
//!
//! **The way there** (`Placer::start`, answering a `Handle` at once and
//! placing on a thread of its own). The model is resolved here (D12): what
//! the call named, else the default a local subagent would run on. The job
//! is packed (`handoff::pack_job`, the prompt in it) and stored under a
//! session whose lease this supervisor holds (`JobStore`): the parent's
//! when it is synced, else one the `Placer` registers for the job alone
//! and lets go once a child started or every pick refused. `run`
//! (`host_command::RunTask`) goes with `notAfterMs` `ANSWER_WAIT` ahead; a
//! refusal, or no answer by then and `ANSWER_GRACE` more, moves to the next
//! pick, once (D11): nothing ran yet. The target never spends the job, so
//! the next pick reads the same one; the `Placer` spends it once a child
//! started. The machines' clocks are taken to agree within `ANSWER_GRACE`
//! (5 s). A `started` that arrives once the `Placer` has moved on
//! (`in_time` false) has its child stopped, through the session's own
//! interrupt as any follower stops a turn, and its worktree left to the
//! target's clean-up: the target holds itself to `notAfterMs`, and this is
//! the backstop when the clocks disagree by more.
//!
//! **The way back** (`Placer::wait`). The child is followed as any synced
//! session is; its bridge, which holds its lease, stores the result and
//! logs `task.result` (`LogBody::TaskResult`) when the one turn completes.
//! Any of the person's devices can write to a log, so the `Placer` takes
//! the result only as `result_of` checks it: in the started child's own
//! session, after its turn's `turn.completed`, its transport a slug and its
//! key 64 hex digits (`host_command::slug_shaped`, `key_shaped`); anything
//! else fails the task. Then it fetches it and lands it in the working
//! tree (`handoff::apply_result`). A failure after the child started is the
//! caller's, never retried (D11): `Failure`. A machine with no announcement
//! for `OFFLINE_AFTER` and no stream from the child has gone (R-ALLOC-9).
//!
//! Slice 07 builds `busy`, `ruled_out`, `choose` and the `Placer`; slice 08
//! calls it from krowk-mcp.

use super::announce::Announcer;
use krowk_client::host_command::{key_shaped, slug_shaped, HostAnswer, JobRef, ModelChoice, RunTask};
use krowk_client::presence::{AnnouncedInstance, Announcement, Capacity};
use krowk_client::protocol::{BudgetLimits, Item, LimitState, LogBody, LogEvent, ModelRef, PermissionMode, TurnStatus, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a placement this supervisor made counts as an agent slot in
/// use on its machine (D9), so calls made together spread out.
pub const RECENT: Duration = Duration::from_secs(30);
/// How far ahead a `run`'s `notAfterMs` is: the host-silence limit `krowk
/// sync new --host` waits (D11).
pub const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// How much longer than `notAfterMs` the supervisor still takes a
/// `started`: one the host sent just in time, still on its way.
pub const ANSWER_GRACE: Duration = Duration::from_secs(5);
/// How long a machine may go unannounced, with no stream from its child,
/// before the task fails as gone (R-ALLOC-9): two announcement lifetimes.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(60);
/// How many finished tasks a `Placer` keeps to be collected.
const MAX_KEPT: usize = 64;
/// Under this much free memory a machine is busy (D5).
pub const MIN_FREE_MEMORY: u64 = 2 << 30;

/// Where a call asks its task to run (R-ALLOC-10). No pin is the default
/// rule: here, unless here is busy (D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPin {
    /// That machine, by its name on the device list.
    Machine(String),
    /// Whichever machine has the most room, this one included.
    Any,
    /// Whichever machine of this OS has the most room: `macos`, `linux` or
    /// `windows`, as the device list names it.
    Os(String),
}

/// The operating systems `any:<os>` names.
pub const OSES: [&str; 3] = ["macos", "linux", "windows"];

impl HostPin {
    /// `desktop`, `any`, `any:macos`, `any:linux` or `any:windows`.
    pub fn parse(s: &str) -> Result<HostPin, String> {
        let s = s.trim();
        match s.split_once(':') {
            _ if s.is_empty() => Err("host is empty: name a machine, or any, any:macos, any:linux or any:windows".into()),
            _ if s == "any" => Ok(HostPin::Any),
            Some(("any", os)) if OSES.contains(&os) => Ok(HostPin::Os(os.to_string())),
            Some(("any", os)) => Err(format!("any:{os} names no OS krowk knows: any:macos, any:linux or any:windows")),
            _ => Ok(HostPin::Machine(s.to_string())),
        }
    }
}

/// A session whose lease this supervisor holds, which a job's transport is
/// stored under (`krowk_api` `put_transport` takes the holder's token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStore {
    pub session: String,
    pub lease_token: String,
}

/// A task to place: what both callers hand `Placer::start`.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSpec {
    /// A few words, shown to the person.
    pub description: String,
    /// The child's one turn.
    pub prompt: String,
    /// The supervisor's working directory: its repository is the task's,
    /// its state the job's, and the result lands in its working tree.
    pub cwd: PathBuf,
    pub host: Option<HostPin>,
    /// What the call named, if anything; resolved by the `Placer` (D12).
    pub model: ModelChoice,
    /// What is left of the supervisor's tree budget (D12).
    pub budget: BudgetLimits,
    /// The parent's own mode, which the child may only be stricter than
    /// (D13): sent as the `run`'s `permissionMode`.
    pub permission_mode: Option<PermissionMode>,
    /// Where to store the job: the parent's synced session and its lease;
    /// none for a parent that is not synced (krowk-mcp's), when the
    /// `Placer` registers one of its own.
    pub store: Option<JobStore>,
}

/// One machine as placement sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Its announcement, opened; for this machine, one the `Placer` makes
    /// from its capacity read now.
    pub announcement: Announcement,
    /// The registry lists it online. This machine always is.
    pub online: bool,
    /// It is the supervisor's own machine.
    pub here: bool,
    /// The placements this supervisor sent it within `RECENT` (D9).
    pub placed_recently: u32,
}

/// What a pick is held to.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// The task's repository, normalised (`presence::normalise_origin`).
    pub origin: &'a str,
    /// Its origin as the supervisor's clone has it, which a fix names
    /// (`git clone <clone_url>`), with any user or password left out.
    pub clone_url: &'a str,
    /// The model, resolved: its instance must be ready on the machine. The
    /// model id is not held to the announced list, which is trimmed first.
    pub model: &'a ModelRef,
    pub host: Option<&'a HostPin>,
}

/// One machine picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// This machine.
    Here,
    /// The candidate at this index, sent a `run`.
    There(usize),
}

/// The picks, and why the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// In order: the first, at most one more machine to send to (D11),
    /// and `Here` last when this machine can take it and is not first.
    pub picks: Vec<Pick>,
    /// Why the first, as the parent's stream says it: "this machine is
    /// busy (3 of 3 build slots in use)", "this machine has room".
    pub why: String,
}

/// Why a machine with `c` is busy (D5), in words: "3 of 3 build slots in
/// use"; none when it has room.
pub fn busy(c: &Capacity) -> Option<String> {
    if c.build_slots.total > 0 && c.build_slots.in_use >= c.build_slots.total {
        return Some(format!("{} of {} build slots in use", c.build_slots.in_use, c.build_slots.total));
    }
    if c.agent_slots.total > 0 && c.agent_slots.in_use >= c.agent_slots.total {
        return Some(format!("{} of {} agent slots in use", c.agent_slots.in_use, c.agent_slots.total));
    }
    if c.cores > 0 && c.load_1m >= f64::from(c.cores) {
        return Some(format!("load {:.1} on {} cores", c.load_1m, c.cores));
    }
    if c.free_memory < MIN_FREE_MEMORY {
        return Some(format!("{:.1} GiB of memory free", c.free_memory as f64 / (1u64 << 30) as f64));
    }
    None
}

/// `url` as a `git clone` is told it: a password or token never, and for
/// `http` and `https` no user either. An `ssh` or scp-style user stays, as
/// `git clone` needs it, since it is no secret.
fn without_credentials(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let Some((user, host)) = authority.rsplit_once('@') else { return url.to_string() };
    let web = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https");
    if web || user.contains(':') {
        format!("{scheme}://{host}/{path}")
    } else {
        url.to_string()
    }
}

/// The model's instance on the machine, when it announces one of that name.
fn instance<'a>(c: &'a Candidate, ask: &Ask<'_>) -> Option<&'a AnnouncedInstance> {
    c.announcement.instances.iter().find(|i| i.name == ask.model.instance)
}

/// Why `c` cannot take `ask` at all (see the module's docs), with the fix
/// where there is one ("run: git clone <clone_url>"); none when it can.
pub fn ruled_out(c: &Candidate, ask: &Ask<'_>) -> Option<String> {
    let a = &c.announcement;
    let name = &a.name;
    if !c.online {
        return Some(format!("{name} is offline"));
    }
    if !c.here {
        let Some(cap) = &a.capacity else {
            return Some(format!("{name} runs a krowk older than automatic allocation: update krowk there"));
        };
        if !a.accepting {
            return Some(format!("{name} does not take work from your other machines: on {name}, run: krowk machines accept on"));
        }
        if !a.repositories.iter().any(|r| r.origin == ask.origin) {
            return Some(format!("{name} has no clone of {}: on {name}, run: git clone {}", ask.origin, without_credentials(ask.clone_url)));
        }
        let taken = cap.agent_slots.in_use.saturating_add(c.placed_recently);
        if cap.agent_slots.total == 0 || taken >= cap.agent_slots.total {
            return Some(format!("{name} has no free agent slot ({taken} of {} in use)", cap.agent_slots.total));
        }
    }
    match instance(c, ask) {
        Some(i) if i.ready => None,
        Some(i) => Some(format!("{name} cannot run {}: {}", ask.model.instance, i.fix.clone().unwrap_or_else(|| format!("{} is not ready ({})", i.name, i.state)))),
        None => Some(format!("{name} has no account {} to run {}: sign one in there with krowk connect", ask.model.instance, ask.model)),
    }
}

/// How a machine ranks among the others (D5, D6): lower sorts first.
fn rank(c: &Candidate, ask: &Ask<'_>) -> (bool, std::cmp::Reverse<u32>, std::cmp::Reverse<u32>, f64, std::cmp::Reverse<u64>) {
    let cap = c.announcement.capacity.unwrap_or_default();
    let near = instance(c, ask).and_then(|i| i.limit.as_ref()).is_some_and(|l| l.status != LimitState::Allowed);
    let agents = cap.agent_slots.total.saturating_sub(cap.agent_slots.in_use).saturating_sub(c.placed_recently);
    let builds = cap.build_slots.total.saturating_sub(cap.build_slots.in_use);
    (near, std::cmp::Reverse(agents), std::cmp::Reverse(builds), cap.load_1m / f64::from(cap.cores.max(1)), std::cmp::Reverse(cap.free_memory))
}

/// `of` sorted best first; machines that rank alike stay in the order given.
fn ranked(of: &[usize], candidates: &[Candidate], ask: &Ask<'_>) -> Vec<usize> {
    let mut v = of.to_vec();
    v.sort_by(|&a, &b| {
        let (x, y) = (rank(&candidates[a], ask), rank(&candidates[b], ask));
        x.0.cmp(&y.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)).then(x.3.partial_cmp(&y.3).unwrap_or(std::cmp::Ordering::Equal)).then(x.4.cmp(&y.4))
    });
    v
}

/// The picks among `candidates` for `ask` (see the module's docs); `Err`
/// is why none can take it, each machine's reason and fix.
pub fn choose(candidates: &[Candidate], ask: &Ask<'_>) -> Result<Choice, String> {
    let named = |c: &Candidate| match ask.host {
        None | Some(HostPin::Any) => true,
        Some(HostPin::Machine(n)) => c.announcement.name.eq_ignore_ascii_case(n),
        Some(HostPin::Os(os)) => c.announcement.os.eq_ignore_ascii_case(os),
    };
    let within: Vec<usize> = (0..candidates.len()).filter(|&i| named(&candidates[i])).collect();
    if within.is_empty() {
        return Err(match ask.host {
            Some(HostPin::Machine(n)) => format!("None of your machines is named {n}. `krowk machines` lists them."),
            Some(HostPin::Os(os)) => format!("None of your machines runs {os}. `krowk machines` lists them."),
            _ => "There is no machine to run it on.".to_string(),
        });
    }
    let mut why_not = Vec::new();
    let mut can = Vec::new();
    for &i in &within {
        match ruled_out(&candidates[i], ask) {
            Some(why) => why_not.push(format!("- {why}")),
            None => can.push(i),
        }
    }
    if can.is_empty() {
        return Err(format!("No machine can take this task:\n{}", why_not.join("\n")));
    }
    let here = can.iter().copied().find(|&i| candidates[i].here);
    let pick = |i: usize| if candidates[i].here { Pick::Here } else { Pick::There(i) };
    match ask.host {
        // A machine by name: that one alone, with no fallback.
        Some(HostPin::Machine(n)) => {
            let i = can[0];
            let why = if candidates[i].here { "you asked for this machine".to_string() } else { format!("you asked for {n}") };
            Ok(Choice { picks: vec![pick(i)], why })
        }
        Some(pin) => {
            let order = ranked(&can, candidates, ask);
            let first = order[0];
            let mut picks = vec![pick(first)];
            if !candidates[first].here {
                if let Some(&second) = order[1..].iter().find(|&&i| !candidates[i].here) {
                    picks.push(Pick::There(second));
                }
                if here.is_some() {
                    picks.push(Pick::Here);
                }
            }
            let what = match pin {
                HostPin::Os(os) => format!("the most room of your {os} machines"),
                _ => "the most room".to_string(),
            };
            let why = if candidates[first].here { format!("this machine has {what}") } else { format!("{} has {what}", candidates[first].announcement.name) };
            Ok(Choice { picks, why })
        }
        None => {
            let here_busy = here.and_then(|i| candidates[i].announcement.capacity.as_ref().and_then(busy));
            if here.is_some() && here_busy.is_none() {
                return Ok(Choice { picks: vec![Pick::Here], why: "this machine has room".into() });
            }
            let others: Vec<usize> = can.iter().copied().filter(|&i| !candidates[i].here).collect();
            let mut picks: Vec<Pick> = ranked(&others, candidates, ask).into_iter().take(2).map(Pick::There).collect();
            if here.is_some() {
                picks.push(Pick::Here);
            }
            let why = match (here_busy, candidates.iter().find(|c| c.here)) {
                (Some(b), _) => format!("this machine is busy ({b})"),
                (None, Some(h)) => format!("this machine cannot take it ({})", ruled_out(h, ask).unwrap_or_default()),
                (None, None) => "the most room".into(),
            };
            Ok(Choice { picks, why })
        }
    }
}

/// Whether a `started` received at `received_ms` (the supervisor's clock)
/// for a `run` whose `notAfterMs` was `not_after_ms` came in time: by
/// `notAfterMs` and `ANSWER_GRACE` more. One that did not has its child
/// stopped (see the module's docs).
pub fn in_time(not_after_ms: i64, received_ms: i64) -> bool {
    in_time_within(not_after_ms, received_ms, ANSWER_GRACE)
}

fn in_time_within(not_after_ms: i64, received_ms: i64, grace: Duration) -> bool {
    received_ms <= not_after_ms.saturating_add(grace.as_millis() as i64)
}

/// A child's result, as its log names it and `result_of` checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRef {
    /// The transport and its key; both none for a child that changed
    /// nothing.
    pub transport: Option<String>,
    pub key: Option<String>,
    pub against: String,
    pub denied: Vec<String>,
}

/// The result of `child` in `events`, its log as followed (R-ALLOC-8):
/// none while its turn runs, and none when its `turn.completed` is there
/// and no `task.result` yet (the bridge logs it once the result is
/// stored). The first `task.result` in the child's session whose `turnId`
/// is the completed turn's is the one taken. `Err`, which fails the task,
/// for a `task.result` in another session, before the turn's
/// `turn.completed` or for another turn, with a transport or key out of
/// shape, or with one and not the other. Beyond these checks a result is
/// not authenticated: it is trusted as the person's own sealed log is,
/// which any of the person's devices can write.
pub fn result_of(events: &[LogEvent], child: &str) -> Result<Option<ResultRef>, String> {
    let mut completed: Option<&str> = None;
    for e in events {
        match &e.body {
            LogBody::TurnCompleted { turn_id, .. } if e.session_id == child => completed = Some(turn_id),
            LogBody::TaskResult { turn_id, transport, key, against, denied } => {
                if e.session_id != child {
                    return Err(format!("a task result from session {}, which is not the child {child}", e.session_id));
                }
                match completed {
                    None => return Err("the child logged its result before its turn completed".into()),
                    Some(t) if t != turn_id => return Err(format!("the child's result is of turn {turn_id}, not of the turn {t} that completed")),
                    Some(_) => {}
                }
                match (transport, key) {
                    (Some(t), Some(k)) if slug_shaped(t) && key_shaped(k) => {}
                    (None, None) => {}
                    _ => return Err("the child's result names a transport or key this krowk does not read".into()),
                }
                return Ok(Some(ResultRef { transport: transport.clone(), key: key.clone(), against: against.clone(), denied: denied.clone() }));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Who runs a task placed on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Here {
    /// The caller, as it runs any subagent (krowk's own agent):
    /// `Progress::Here` says so.
    Caller,
    /// The `Placer`, as a child in a local worktree of the repository,
    /// landed as a remote one is (krowk-mcp, which has no engine of its
    /// own). Never through the registry.
    Worktree,
}

/// A task started: its id, `krowk_collect`'s argument, minted at once, and
/// the working tree its result lands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub id: String,
    pub cwd: PathBuf,
}

/// Where a task is.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Still being placed. `krowk_collect` says it is still running, as it
    /// does for `Running`.
    Placing,
    /// Placed on this machine, for the caller to run (`Here::Caller`).
    Here { why: String },
    /// Running on `machine` as session `child`.
    Running { machine: String, child: String, why: String },
    /// Over, its result landed.
    Done(Outcome),
}

/// A finished task (R-ALLOC-8): what the caller's tool result says.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The child's final message.
    pub summary: String,
    /// The files applied to the working tree.
    pub applied: Vec<String>,
    /// When its changes were not applied: the line naming the files and the
    /// worktree they are kept in (`worktree::apply::Applied::note`).
    pub kept: Option<String>,
    /// The approvals denied because a remote child cannot ask (R-ALLOC-14).
    pub denied: Vec<String>,
    pub usage: Usage,
    /// The machine it ran on.
    pub machine: String,
}

/// Why a task did not run, or did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No machine can take it: why, each machine's reason and fix.
    NoMachine(String),
    /// Every candidate left refused it as over the person's machine limit (D15).
    MachineLimit,
    /// The job is past `handoff::MAX_JOB`.
    TooBig,
    /// The machine went offline mid-task (R-ALLOC-9).
    Offline { machine: String },
    /// After the child started: the machine and the reason (D11).
    Failed { machine: String, reason: String },
    /// No task by that id.
    Unknown(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoMachine(why) => write!(f, "{why}"),
            Failure::MachineLimit => write!(f, "No machine could take it without going over your plan's machine limit. See krowk billing."),
            Failure::TooBig => write!(f, "{}", super::handoff::JOB_TOO_BIG),
            Failure::Offline { machine } => write!(f, "{machine} went offline mid-task; its work is in a worktree there."),
            Failure::Failed { machine, reason } => write!(f, "The task failed on {machine}: {reason}"),
            Failure::Unknown(id) => write!(f, "No task {id} was started here."),
        }
    }
}

/// How long placement waits and polls. The constants above are what it
/// runs with; a test makes them short.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub recent: Duration,
    pub answer_wait: Duration,
    pub answer_grace: Duration,
    pub offline_after: Duration,
    /// How often a running child's log and its machine's announcement are
    /// looked at.
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing { recent: RECENT, answer_wait: ANSWER_WAIT, answer_grace: ANSWER_GRACE, offline_after: OFFLINE_AFTER, poll: Duration::from_secs(2) }
    }
}

/// A child run in a local worktree (`Here::Worktree`): what it said and
/// spent, and the worktree its changes are in, which the `Placer` lands.
#[derive(Debug)]
pub struct LocalRun {
    pub summary: String,
    pub usage: Usage,
    pub worktree: crate::worktree::Worktree,
    pub denied: Vec<String>,
}

/// What the `Placer` needs of the world outside it: this machine, the
/// registry and the other machines. `fleet::Registry` is the real one;
/// the tests' is a fake. All of it blocks.
pub trait Fleet: Send + Sync {
    /// This device's id, in hex: the `run`'s `requestedBy`.
    fn device(&self) -> String;
    /// The model a call named, else the local subagent's default (D12).
    fn resolve_model(&self, asked: &ModelChoice) -> Result<ModelRef, String>;
    /// This machine as placement sees it, its capacity read now, with the
    /// instance of `model` ready: the caller resolved it here.
    fn here(&self, model: &ModelRef) -> Candidate;
    /// Every other machine as it last announced, `placed_recently` unset;
    /// `Err` is why the registry could not be read.
    fn others(&self) -> Result<Vec<Candidate>, String>;
    /// The repository `cwd` is in: its origin normalised, and as the clone
    /// has it; none when it has no origin another machine could clone.
    fn repository(&self, cwd: &std::path::Path) -> Option<(String, String)>;
    /// A session this supervisor holds the lease of, to store a job under:
    /// `given` when there is one, else a short-lived one made for the job.
    fn open_store(&self, given: Option<&JobStore>) -> Result<JobStore, String>;
    /// Lets go of a session `open_store` made.
    fn close_store(&self, store: &JobStore);
    fn pack_job(&self, cwd: &std::path::Path, session: &str, prompt: &str) -> Result<super::handoff::Bundle, String>;
    /// Seals the job, stores it under `store`, and says where.
    fn put_job(&self, store: &JobStore, job: &super::handoff::Bundle) -> Result<JobRef, String>;
    fn spend_job(&self, job: &JobRef);
    /// Sends `task` to the machine and waits up to `wait` for its answer:
    /// none when it did not answer. `Err` is why it could not be sent.
    fn send_run(&self, to: &Announcement, task: &RunTask, wait: Duration) -> Result<Option<HostAnswer>, String>;
    /// The child's log so far, as events.
    fn watch(&self, child: &str) -> Result<Vec<Value>, String>;
    /// How long ago the machine last announced; none when it never has.
    fn seen(&self, device: &str) -> Option<Duration>;
    /// The result `task.result` named, opened.
    fn read_result(&self, child: &str, transport: &str, key: &str) -> Result<super::handoff::Bundle, String>;
    /// `handoff::apply_result`, into `to`.
    fn land(&self, job: &super::handoff::Bundle, result: &super::handoff::Bundle, to: &std::path::Path) -> Result<(crate::worktree::Worktree, crate::worktree::apply::Applied), String>;
    /// Stops the child `child` through its session's own interrupt, as any
    /// follower stops a turn. Used on a child that started too late.
    fn stop(&self, _child: &str) -> Result<(), String> {
        Err("this machine has no way to stop a child".into())
    }
    /// Runs the task as a child in a local worktree (`Here::Worktree`).
    fn run_local(&self, _spec: &TaskSpec, _model: &ModelRef) -> Result<LocalRun, String> {
        Err("this machine has no way to run a child of its own here".into())
    }
    /// A local child's worktree landed in `to`; none when it changed nothing.
    fn land_local(&self, wt: &crate::worktree::Worktree, to: &std::path::Path) -> Result<Option<crate::worktree::apply::Applied>, String> {
        crate::worktree::finish_into(wt, to).map_err(|e| e.to_string())
    }
}

/// Where a task stands, as the `Placer` keeps it.
#[derive(Debug, Clone, Default)]
enum State {
    #[default]
    Placing,
    Here(String),
    Running { machine: String, child: String, why: String },
    Done(Result<Outcome, Failure>),
}

#[derive(Default)]
struct Task {
    state: Mutex<State>,
    changed: Condvar,
    /// The caller gave up (its turn was interrupted): the child is stopped
    /// and nothing lands.
    cancelled: std::sync::atomic::AtomicBool,
}

impl Task {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s;
        self.changed.notify_all();
    }
}

/// A job's session let go of when placement is over with it, if the
/// `Placer` made it.
struct Release<'a>(&'a dyn Fleet, Option<JobStore>);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.1.take() {
            self.0.close_store(&s);
        }
    }
}

/// The line the parent's stream carries for where a task runs: `Running on
/// desktop: this machine is busy (3 of 3 build slots in use).` or `Running
/// here: this machine has room.` None while it is being placed or once done.
pub fn stream_line(p: &Progress) -> Option<String> {
    match p {
        Progress::Here { why } => Some(format!("Running here: {why}.")),
        Progress::Running { machine, why, .. } => Some(format!("Running on {machine}: {why}.")),
        Progress::Placing | Progress::Done(_) => None,
    }
}

/// The supervisor's side: places tasks and brings their results back. One
/// per supervisor, kept for its life, so its placements of the last
/// `RECENT` count (D9) and its tasks can be collected. Blocking: an async
/// caller waits on a blocking thread.
pub struct Placer {
    fleet: Box<dyn Fleet>,
    timing: Timing,
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    /// Each placement sent, by the device it went to, while it counts as
    /// an agent slot in use there (D9).
    recent: Mutex<Vec<(String, Instant)>>,
}

impl Placer {
    /// A placer over the registry `announcer` reaches, with the default
    /// pools and model resolution (`fleet::Registry::new`).
    pub fn new(announcer: Announcer) -> Arc<Placer> {
        Placer::with(Box::new(super::fleet::Registry::new(announcer)), Timing::default())
    }

    pub fn with(fleet: Box<dyn Fleet>, timing: Timing) -> Arc<Placer> {
        Arc::new(Placer { fleet, timing, tasks: Mutex::default(), recent: Mutex::default() })
    }

    /// `spec` handed over: its `Handle` at once, the placing done on a
    /// thread of its own, `here` running a pick of this machine.
    pub fn start(self: &Arc<Self>, spec: TaskSpec, here: Here) -> Handle {
        let id = krowk_store::new_id();
        let task = Arc::new(Task::default());
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            // Finished tasks are kept to be collected, up to a bound.
            // The oldest first: ids sort by the time they were made.
            if tasks.len() >= MAX_KEPT {
                let mut done: Vec<String> = tasks.iter().filter(|(_, t)| matches!(*t.state.lock().unwrap_or_else(|e| e.into_inner()), State::Done(_))).map(|(id, _)| id.clone()).collect();
                done.sort();
                for id in done.into_iter().take(tasks.len() + 1 - MAX_KEPT) {
                    tasks.remove(&id);
                }
            }
            tasks.insert(id.clone(), task.clone());
        }
        let handle = Handle { id, cwd: spec.cwd.clone() };
        let me = self.clone();
        let t = task.clone();
        let spawned = std::thread::Builder::new().name("krowk-placing".into()).spawn(move || {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.drive(&t, spec, here)));
            match ran {
                Ok(Ok(Some(outcome))) => t.set(State::Done(Ok(outcome))),
                // Placed here for the caller to run: already said.
                Ok(Ok(None)) => {}
                Ok(Err(f)) => t.set(State::Done(Err(f))),
                Err(_) => t.set(State::Done(Err(Failure::Failed { machine: "this machine".into(), reason: "placement stopped unexpectedly".into() }))),
            }
        });
        if let Err(e) = spawned {
            task.set(State::Done(Err(Failure::Failed { machine: "this machine".into(), reason: format!("placement could not start: {e}") })));
        }
        handle
    }

    /// Up to `most` for the task to move past placing and running: where
    /// it is then, its result landed in `h.cwd` once done, taken only as
    /// `result_of` checks it.
    pub fn wait(&self, h: &Handle, most: Duration) -> Result<Progress, Failure> {
        self.until(h, most, false)
    }

    /// The caller no longer wants the task (its turn was interrupted): a
    /// child that started is stopped, and its result is not landed.
    pub fn cancel(&self, h: &Handle) {
        if let Some(t) = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).get(&h.id) {
            t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Up to `most` for the task to be placed: where it is then, which is
    /// `Placing` still only if the time ran out.
    pub fn placed(&self, h: &Handle, most: Duration) -> Result<Progress, Failure> {
        self.until(h, most, true)
    }

    fn until(&self, h: &Handle, most: Duration, placed_is_enough: bool) -> Result<Progress, Failure> {
        let task = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).get(&h.id).cloned().ok_or_else(|| Failure::Unknown(h.id.clone()))?;
        let deadline = Instant::now() + most;
        let mut state = task.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &*state {
                State::Done(r) => return r.clone().map(Progress::Done),
                State::Here(why) => return Ok(Progress::Here { why: why.clone() }),
                State::Running { machine, child, why } if placed_is_enough => return Ok(Progress::Running { machine: machine.clone(), child: child.clone(), why: why.clone() }),
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(match &*state {
                    State::Running { machine, child, why } => Progress::Running { machine: machine.clone(), child: child.clone(), why: why.clone() },
                    _ => Progress::Placing,
                });
            }
            state = task.changed.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The placements sent `device` within `recent`, and the older ones
    /// forgotten.
    fn recently(&self, device: &str) -> u32 {
        let mut r = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        r.retain(|(_, at)| at.elapsed() < self.timing.recent);
        r.iter().filter(|(d, _)| d == device).count() as u32
    }

    fn placed_on(&self, device: &str) {
        self.recent.lock().unwrap_or_else(|e| e.into_inner()).push((device.to_string(), Instant::now()));
    }

    /// Places the task and follows it: `None` when it is for the caller to
    /// run here, else its outcome with the result landed.
    fn drive(&self, task: &Task, spec: TaskSpec, here_mode: Here) -> Result<Option<Outcome>, Failure> {
        let f = &*self.fleet;
        let fail_here = |why: String| -> Result<Option<Outcome>, Failure> { Err(Failure::Failed { machine: "this machine".into(), reason: why }) };
        let model = match f.resolve_model(&spec.model) {
            Ok(m) => m,
            Err(why) => return fail_here(why),
        };
        let here = f.here(&model);
        let here_name = here.announcement.name.clone();
        let repository = f.repository(&spec.cwd);
        let (origin, clone_url) = repository.clone().unwrap_or_default();
        let ask = Ask { origin: &origin, clone_url: &clone_url, model: &model, host: spec.host.as_ref() };
        let room = ruled_out(&here, &ask).is_none() && here.announcement.capacity.as_ref().and_then(busy).is_none();
        // The usual case reads nothing from the registry (D4).
        if spec.host.is_none() && room {
            return self.at_home(task, &spec, here_mode, &model, &here_name, "this machine has room".into());
        }
        let elsewhere = match &spec.host {
            Some(HostPin::Machine(n)) => !here_name.eq_ignore_ascii_case(n),
            Some(HostPin::Os(os)) => !here.announcement.os.eq_ignore_ascii_case(os),
            _ => false,
        };
        if repository.is_none() && elsewhere {
            return Err(Failure::NoMachine("This repository has no origin remote another machine could clone, so the task can only run here.".into()));
        }
        let mut all = vec![here];
        let mut unread = None;
        if repository.is_some() {
            match f.others() {
                Ok(others) => {
                    for mut c in others {
                        c.placed_recently = self.recently(&c.announcement.device);
                        all.push(c);
                    }
                }
                Err(e) => unread = Some(e),
            }
        }
        let choice = choose(&all, &ask).map_err(|mut why| {
            if let Some(e) = &unread {
                why.push_str(&format!("\nYour other machines could not be read ({e})."));
            }
            Failure::NoMachine(why)
        })?;
        if choice.picks[0] == Pick::Here {
            return self.at_home(task, &spec, here_mode, &model, &here_name, choice.why);
        }

        let t = &self.timing;
        let own = spec.store.is_none();
        let failed = |reason: String| Failure::Failed { machine: "this machine".into(), reason };
        let store = f.open_store(spec.store.as_ref()).map_err(&failed)?;
        let mut release = Some(Release(f, own.then(|| store.clone())));
        if task.is_cancelled() {
            return Err(failed("the task was interrupted".into()));
        }
        let bundle = f.pack_job(&spec.cwd, &store.session, &spec.prompt).map_err(|e| if e == super::handoff::JOB_TOO_BIG { Failure::TooBig } else { failed(e) })?;
        let job = f.put_job(&store, &bundle).map_err(&failed)?;
        if task.is_cancelled() {
            f.spend_job(&job);
            return Err(failed("the task was interrupted".into()));
        }
        let mut refused: Vec<(String, String, String)> = Vec::new();
        let mut why = choice.why.clone();
        for pick in &choice.picks {
            if task.is_cancelled() {
                f.spend_job(&job);
                return Err(failed("the task was interrupted".into()));
            }
            let i = match pick {
                Pick::Here => {
                    drop(release.take());
                    let says = refused.last().map_or(why.clone(), |(n, _, m)| format!("{n} would not take it ({m})"));
                    return self.at_home(task, &spec, here_mode, &model, &here_name, says);
                }
                Pick::There(i) => *i,
            };
            let to = &all[i].announcement;
            let name = to.name.clone();
            let not_after_ms = now_ms() + t.answer_wait.as_millis() as i64;
            let run = RunTask { origin: origin.clone(), model: model.clone(), job: job.clone(), budget: spec.budget, requested_by: f.device(), permission_mode: spec.permission_mode, not_after_ms };
            match f.send_run(to, &run, t.answer_wait + t.answer_grace) {
                Ok(Some(HostAnswer::Started { session_id })) if crate::log::valid_id(&session_id) && !in_time_within(not_after_ms, now_ms(), t.answer_grace) => {
                    // Placement has moved on by now (D11): this child
                    // would run beside the next pick's.
                    self.placed_on(&to.device);
                    let stopped = match f.stop(&session_id) {
                        Ok(()) => "stopped".to_string(),
                        Err(e) => format!("it could not be stopped: {e}"),
                    };
                    refused.push((name, "late".into(), format!("it started the task after {} s, once placement had moved on ({stopped})", t.answer_wait.as_secs())));
                }
                Ok(Some(HostAnswer::Started { session_id })) if crate::log::valid_id(&session_id) => {
                    self.placed_on(&to.device);
                    f.spend_job(&job);
                    drop(release.take());
                    if let Some((prev, _, m)) = refused.last() {
                        why = format!("{prev} would not take it ({m})");
                    }
                    task.set(State::Running { machine: name.clone(), child: session_id.clone(), why });
                    if task.is_cancelled() {
                        let _ = f.stop(&session_id);
                        return Err(Failure::Failed { machine: name, reason: "the task was interrupted".into() });
                    }
                    return self.follow(task, &spec, &bundle, &name, &to.device, &session_id).map(Some);
                }
                Ok(Some(HostAnswer::Started { session_id })) => refused.push((name, "bad_answer".into(), format!("it answered with {session_id:?}, which is no session id"))),
                Ok(Some(HostAnswer::Refused { code, message })) => refused.push((name, code, message)),
                Ok(Some(HostAnswer::Taken { .. })) => refused.push((name, "bad_answer".into(), "it answered the task as a move".into())),
                Ok(None) => {
                    // It may still start it, so it counts as a slot taken.
                    self.placed_on(&to.device);
                    refused.push((name, "no_answer".into(), format!("it did not answer within {} s", t.answer_wait.as_secs())));
                }
                Err(e) => refused.push((name, "unreachable".into(), e)),
            }
        }
        drop(release.take());
        if refused.iter().all(|(_, code, _)| code == "machine_limit") {
            return Err(Failure::MachineLimit);
        }
        let lines: Vec<String> = refused.iter().map(|(n, _, m)| format!("- {n}: {m}")).collect();
        Err(Failure::NoMachine(format!("No machine took the task:\n{}", lines.join("\n"))))
    }

    /// The task placed on this machine: for the caller to run, or run here
    /// by the `Placer` in a worktree.
    fn at_home(&self, task: &Task, spec: &TaskSpec, mode: Here, model: &ModelRef, name: &str, why: String) -> Result<Option<Outcome>, Failure> {
        let f = &*self.fleet;
        if mode == Here::Caller {
            task.set(State::Here(why));
            return Ok(None);
        }
        task.set(State::Running { machine: name.to_string(), child: String::new(), why });
        let failed = |reason: String| Failure::Failed { machine: name.to_string(), reason };
        let ran = f.run_local(spec, model).map_err(&failed)?;
        let landed = f.land_local(&ran.worktree, &spec.cwd).map_err(|e| failed(format!("its changes could not be applied: {e}")))?;
        let (applied, kept) = split(&ran.worktree, landed);
        Ok(Some(Outcome { summary: ran.summary, applied, kept, denied: ran.denied, usage: ran.usage, machine: name.to_string() }))
    }

    /// Follows the child `child` on `machine` until its one turn is over,
    /// then lands its result (R-ALLOC-8).
    fn follow(&self, task: &Task, spec: &TaskSpec, job: &super::handoff::Bundle, machine: &str, device: &str, child: &str) -> Result<Outcome, Failure> {
        let f = &*self.fleet;
        let t = &self.timing;
        let failed = |reason: String| Failure::Failed { machine: machine.to_string(), reason };
        let mut alive = Instant::now();
        let mut read = Instant::now();
        let mut ended: Option<Instant> = None;
        let mut seen_events = 0;
        let done = loop {
            std::thread::sleep(t.poll);
            if task.is_cancelled() {
                let _ = f.stop(child);
                return Err(failed("the task was interrupted".into()));
            }
            match f.watch(child) {
                Ok(events) => {
                    read = Instant::now();
                    if events.len() != seen_events {
                        seen_events = events.len();
                        alive = Instant::now();
                    }
                    match finished(&events, child) {
                        Some(Ok(done)) => break done,
                        Some(Err(why)) => return Err(failed(why)),
                        // Its result is logged right after its turn: a
                        // bridge that went before it did is no result.
                        None if turn_ended(&events, child) && ended.get_or_insert_with(Instant::now).elapsed() >= t.offline_after => {
                            return Err(failed("its turn ended but it logged no result".into()));
                        }
                        None => {}
                    }
                }
                Err(e) if read.elapsed() >= t.offline_after * 3 => return Err(failed(format!("its log could not be read: {e}"))),
                Err(_) => {}
            }
            if f.seen(device).is_some_and(|ago| ago < t.offline_after) {
                alive = Instant::now();
            }
            if alive.elapsed() >= t.offline_after {
                return Err(Failure::Offline { machine: machine.to_string() });
            }
        };
        let (applied, kept) = match (&done.transport, &done.key) {
            (Some(transport), Some(key)) => {
                let result = f.read_result(child, transport, key).map_err(|e| failed(format!("its result could not be read: {e}")))?;
                if task.is_cancelled() {
                    return Err(failed("the task was interrupted".into()));
                }
                let (wt, landed) = f.land(job, &result, &spec.cwd).map_err(|e| failed(format!("its result could not be applied: {e}")))?;
                // Spent only once landed: a failed landing can read it again.
                f.spend_job(&JobRef { session: child.to_string(), transport: transport.clone(), key: key.clone() });
                split(&wt, Some(landed))
            }
            _ => (Vec::new(), None),
        };
        Ok(Outcome { summary: done.summary, applied, kept, denied: done.denied, usage: done.usage, machine: machine.to_string() })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The files applied and the note on what was kept, from what landing
/// `wt` came to.
fn split(wt: &crate::worktree::Worktree, landed: Option<crate::worktree::apply::Applied>) -> (Vec<String>, Option<String>) {
    use crate::worktree::apply::Applied as A;
    match landed {
        None => (Vec::new(), None),
        Some(A::Applied(files)) => (files, None),
        Some(A::AppliedLeft(files, why)) => {
            let note = A::AppliedLeft(files.clone(), why).note(wt);
            (files, note)
        }
        Some(a) => (Vec::new(), a.note(wt)),
    }
}

/// What a child's log says once its one turn is over: its summary, usage,
/// and the result `task.result` names; or why it did not complete. None
/// while it runs, and between its `turn.completed` and its `task.result`.
struct Finish {
    summary: String,
    usage: Usage,
    transport: Option<String>,
    key: Option<String>,
    denied: Vec<String>,
}

/// Whether the child's turn has completed, in whatever way.
fn turn_ended(events: &[Value], child: &str) -> bool {
    events.iter().filter_map(|e| serde_json::from_value::<LogEvent>(e.clone()).ok()).any(|e| e.session_id == child && matches!(e.body, LogBody::TurnCompleted { .. }))
}

fn finished(events: &[Value], child: &str) -> Option<Result<Finish, String>> {
    let events: Vec<LogEvent> = events.iter().filter_map(|e| serde_json::from_value::<LogEvent>(e.clone()).ok()).collect();
    let mut summary = String::new();
    let mut turn = None;
    for e in events.iter().filter(|e| e.session_id == child) {
        match &e.body {
            LogBody::ItemCompleted { item: Item::AssistantText { text }, .. } => summary = text.clone(),
            LogBody::TurnCompleted { status, usage, error, .. } => turn = Some((*status, *usage, error.clone())),
            _ => {}
        }
    }
    let (status, usage, error) = turn?;
    if status != TurnStatus::Completed {
        let why = error.map_or_else(|| "its turn was interrupted".to_string(), |e| e.message);
        return Some(Err(why));
    }
    match result_of(&events, child) {
        Err(e) => Some(Err(e)),
        Ok(None) => None,
        Ok(Some(r)) => Some(Ok(Finish { summary, usage, transport: r.transport, key: r.key, denied: r.denied })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krowk_client::presence::{AnnouncedInstance, AnnouncedRepository, Slots};
    use krowk_client::protocol::ModelRef;
    use krowk_client::protocol::{LimitState, LimitStatus};

    const ORIGIN: &str = "github.com/acme/app";

    /// A machine named `name` on linux, online, accepting, with a clone of
    /// `ORIGIN`, a ready `anthropic` instance and `capacity`: 8 cores, load
    /// 1, 16 GiB free, 0 of 4 build slots and 0 of 8 agent slots in use.
    fn machine(name: &str, here: bool) -> Candidate {
        let capacity = Capacity { cores: 8, load_1m: 1.0, free_memory: 16 << 30, build_slots: Slots { in_use: 0, total: 4 }, agent_slots: Slots { in_use: 0, total: 8 } };
        let instance = AnnouncedInstance { name: "anthropic".into(), kind: "anthropic".into(), ready: true, state: "ready".into(), fix: None, models: vec!["claude-opus-5-5".into()], limit: None };
        let announcement = Announcement {
            device: format!("{name}-id"),
            name: name.into(),
            os: "linux".into(),
            version: "dev".into(),
            online_since_ms: 0,
            instances: vec![instance],
            directories: vec![format!("/home/ann/{name}/app")],
            sessions: Vec::new(),
            capacity: Some(capacity),
            repositories: vec![AnnouncedRepository { directory: format!("/home/ann/{name}/app"), origin: ORIGIN.into() }],
            accepting: true,
        };
        Candidate { announcement, online: true, here, placed_recently: 0 }
    }

    fn cap(c: &mut Candidate) -> &mut Capacity {
        c.announcement.capacity.as_mut().unwrap()
    }

    fn anthropic() -> ModelRef {
        ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() }
    }

    fn ask<'a>(model: &'a ModelRef, host: Option<&'a HostPin>) -> Ask<'a> {
        Ask { origin: ORIGIN, clone_url: "https://github.com/acme/app.git", model, host }
    }

    /// R-ALLOC-10: a host is a machine's name, any, or an OS filter.
    #[test]
    fn r_alloc_10_a_host_pin_reads_as_a_name_any_or_an_os() {
        assert_eq!(HostPin::parse("desktop"), Ok(HostPin::Machine("desktop".into())));
        assert_eq!(HostPin::parse("any"), Ok(HostPin::Any));
        for os in OSES {
            assert_eq!(HostPin::parse(&format!("any:{os}")), Ok(HostPin::Os(os.into())));
        }
        assert!(HostPin::parse("any:beos").unwrap_err().contains("any:macos"));
        assert!(HostPin::parse(" ").is_err());
    }

    /// R-ALLOC-9, D15: the failures say what the spec's screens say.
    #[test]
    fn r_alloc_9_failures_read_as_the_screens_say() {
        assert_eq!(Failure::Offline { machine: "desktop".into() }.to_string(), "desktop went offline mid-task; its work is in a worktree there.");
        assert!(Failure::MachineLimit.to_string().contains("krowk billing"));
        assert!(Failure::TooBig.to_string().contains("64 MiB"));
        assert!(Failure::Unknown("t1".into()).to_string().contains("t1"));
    }

    /// Slice 07 (D5): each of the four ways a machine is busy, said in
    /// words, and none when it has room.
    #[test]
    fn alloc_07_a_machine_is_busy_in_each_of_the_four_ways() {
        let mut m = machine("m", true);
        assert_eq!(busy(cap(&mut m)), None, "room");
        cap(&mut m).build_slots.in_use = 4;
        assert!(busy(cap(&mut m)).unwrap().contains("4 of 4 build slots in use"));
        let mut m = machine("m", true);
        cap(&mut m).agent_slots.in_use = 8;
        assert!(busy(cap(&mut m)).unwrap().contains("8 of 8 agent slots in use"));
        let mut m = machine("m", true);
        cap(&mut m).load_1m = 8.0;
        assert!(busy(cap(&mut m)).is_some(), "load at the core count");
        let mut m = machine("m", true);
        cap(&mut m).free_memory = MIN_FREE_MEMORY - 1;
        assert!(busy(cap(&mut m)).is_some(), "under 2 GiB free");
    }

    /// Slice 07 (D6, D7, D9, D14): each reason a machine is ruled out, the
    /// missing clone with its fix.
    #[test]
    fn alloc_07_a_machine_is_ruled_out_for_each_reason() {
        let none = anthropic();
        assert_eq!(ruled_out(&machine("m", false), &ask(&none, None)), None);
        let mut m = machine("m", false);
        m.online = false;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "offline");
        let mut m = machine("m", false);
        m.announcement.accepting = false;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "not accepting");
        let mut m = machine("m", false);
        m.announcement.repositories.clear();
        assert!(ruled_out(&m, &ask(&none, None)).unwrap().contains("git clone https://github.com/acme/app.git"), "no clone, and the fix");
        let token = Ask { clone_url: "https://ann:ghp_secret@github.com/acme/app.git", ..ask(&none, None) };
        let fix = ruled_out(&m, &token).unwrap();
        assert!(fix.contains("git clone https://github.com/acme/app.git") && !fix.contains("ghp_secret") && !fix.contains("ann@"), "never a credential: {fix}");
        let codex = ModelRef { instance: "codex".into(), model: "gpt-5.5".into() };
        assert!(ruled_out(&machine("m", false), &ask(&codex, None)).is_some(), "no ready instance for the model");
        let mut m = machine("m", false);
        cap(&mut m).agent_slots.in_use = 7;
        m.placed_recently = 1;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "its last free agent slot is one this supervisor just placed on");
        let mut m = machine("m", false);
        m.announcement.capacity = None;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "a host older than allocation");
        let mut here = machine("here", true);
        here.announcement.accepting = false;
        here.announcement.repositories.clear();
        assert_eq!(ruled_out(&here, &ask(&none, None)), None, "neither accepting nor a clone applies to this machine");
        assert!(ruled_out(&here, &ask(&codex, None)).is_some(), "the model does");
    }

    /// Slice 07 (D4, D5, D6): here unless here is busy; then free agent
    /// slots, free build slots, load per core, free memory, and a machine
    /// near its usage limit last; the pick and one fallback.
    #[test]
    fn alloc_07_work_stays_here_unless_busy_then_goes_to_the_most_room() {
        let none = anthropic();
        let (here, b, c) = (machine("here", true), machine("b", false), machine("c", false));
        let roomy = choose(&[here.clone(), b.clone(), c.clone()], &ask(&none, None)).unwrap();
        assert_eq!((roomy.picks[0], roomy.why.as_str()), (Pick::Here, "this machine has room"));

        let mut busy_here = here.clone();
        cap(&mut busy_here).build_slots.in_use = 4;
        let mut fewer_agents = b.clone();
        cap(&mut fewer_agents).agent_slots.in_use = 3;
        let got = choose(&[busy_here.clone(), fewer_agents.clone(), c.clone()], &ask(&none, None)).unwrap();
        assert_eq!(got.picks, [Pick::There(2), Pick::There(1), Pick::Here], "most free agent slots first, one more machine, then here");
        assert!(got.why.contains("4 of 4 build slots in use"), "{}", got.why);

        let mut fewer_builds = b.clone();
        cap(&mut fewer_builds).build_slots.in_use = 1;
        assert_eq!(choose(&[busy_here.clone(), fewer_builds, c.clone()], &ask(&none, None)).unwrap().picks[0], Pick::There(2), "then free build slots");
        let mut loaded = b.clone();
        cap(&mut loaded).load_1m = 4.0;
        assert_eq!(choose(&[busy_here.clone(), loaded, c.clone()], &ask(&none, None)).unwrap().picks[0], Pick::There(2), "then load per core");
        let mut short = b.clone();
        cap(&mut short).free_memory = 4 << 30;
        assert_eq!(choose(&[busy_here.clone(), short, c.clone()], &ask(&none, None)).unwrap().picks[0], Pick::There(2), "then free memory");
        let mut limited = machine("b", false);
        limited.announcement.instances[0].limit = Some(LimitStatus { status: LimitState::Warning, window: None, used_percent: Some(90.0), resets_at_ms: None });
        cap(&mut limited).agent_slots.in_use = 0;
        let mut fuller = c.clone();
        cap(&mut fuller).agent_slots.in_use = 4;
        assert_eq!(choose(&[busy_here.clone(), limited, fuller], &ask(&none, None)).unwrap().picks[0], Pick::There(2), "an account near its limit makes a machine less preferred");

        assert_eq!(choose(&[busy_here.clone(), b.clone()], &ask(&none, None)).unwrap().picks, [Pick::There(1), Pick::Here], "the only other machine refusing: here");
        assert_eq!(choose(&[busy_here], &ask(&none, None)).unwrap().picks, [Pick::Here], "busy, but no other machine can take it: here");
    }

    /// Slice 07 (D11): a `started` counts by `notAfterMs` and the grace;
    /// one later has its child stopped.
    #[test]
    fn alloc_07_a_started_after_the_grace_is_late() {
        let grace = ANSWER_GRACE.as_millis() as i64;
        assert!(in_time(30_000, 29_000));
        assert!(in_time(30_000, 30_000 + grace));
        assert!(!in_time(30_000, 30_001 + grace));
    }

    /// Slice 07 (R-ALLOC-8): a result is taken only from the started
    /// child's own log, after its turn completed, in shape.
    #[test]
    fn alloc_07_a_result_is_taken_only_from_the_child_after_its_turn() {
        use krowk_client::protocol::{LogBody, TurnStatus};
        const CHILD: &str = "0190f3c2-0000-7000-8000-00000000000c";
        let ev = |session: &str, body: LogBody| LogEvent { id: "0190f3c2-0000-7000-8000-0000000000e0".into(), parent_id: None, session_id: session.into(), time_ms: 0, body };
        let done = || ev(CHILD, LogBody::TurnCompleted { turn_id: "t".into(), status: TurnStatus::Completed, usage: Usage::default(), duration_ms: 1, reported_cost_usd: None, reported_tokens: None, error: None });
        let result = |session: &str, transport: Option<&str>, key: Option<String>| ev(session, LogBody::TaskResult { turn_id: "t".into(), transport: transport.map(String::from), key, against: "abc".into(), denied: vec!["bash: rm".into()], error: None });
        let key = || Some("ab".repeat(32));
        assert_eq!(result_of(&[], CHILD), Ok(None), "its turn still runs");
        assert_eq!(result_of(&[done()], CHILD), Ok(None), "its turn completed, its result not stored yet");
        let got = result_of(&[done(), result(CHILD, Some("art_1"), key())], CHILD).unwrap().unwrap();
        assert_eq!((got.transport.as_deref(), got.against.as_str(), got.denied.len()), (Some("art_1"), "abc", 1));
        assert_eq!(result_of(&[done(), result(CHILD, None, None)], CHILD).unwrap().unwrap().transport, None, "an unchanged child");
        assert!(result_of(&[done(), result("0190f3c2-0000-7000-8000-0000000000ff", Some("art_1"), key())], CHILD).is_err(), "another session's");
        assert!(result_of(&[result(CHILD, Some("art_1"), key()), done()], CHILD).is_err(), "before its turn completed");
        assert!(result_of(&[done(), result(CHILD, Some("../x"), key())], CHILD).is_err(), "a transport out of shape");
        assert!(result_of(&[done(), result(CHILD, Some("art_1"), Some("zz".repeat(32)))], CHILD).is_err(), "a key out of shape");
        let other_turn = ev(CHILD, LogBody::TaskResult { turn_id: "u".into(), transport: Some("art_1".into()), key: key(), against: "abc".into(), denied: Vec::new(), error: None });
        assert!(result_of(&[done(), other_turn], CHILD).is_err(), "for another turn");
        let first = result_of(&[done(), result(CHILD, Some("art_1"), key()), result(CHILD, Some("art_2"), key())], CHILD).unwrap().unwrap();
        assert_eq!(first.transport.as_deref(), Some("art_1"), "the first matching one");
        assert!(result_of(&[done(), result(CHILD, Some("art_1"), None)], CHILD).is_err(), "a transport with no key");
        let other_turn = ev(CHILD, LogBody::TaskResult { turn_id: "other".into(), transport: None, key: None, against: "abc".into(), denied: Vec::new() });
        assert!(result_of(&[done(), other_turn], CHILD).is_err(), "a result of another turn");
    }

    /// Slice 07 (D9): this supervisor's recent placements fill a machine's
    /// agent slots, added without overflowing.
    #[test]
    fn alloc_07_recent_placements_add_without_overflow() {
        let none = anthropic();
        let mut m = machine("m", false);
        cap(&mut m).agent_slots.in_use = 1;
        m.placed_recently = u32::MAX;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "full, not wrapped round to free");
    }

    /// Slice 07 (R-ALLOC-10): a pin to a machine by name, `any`, and an OS
    /// filter.
    #[test]
    fn alloc_07_host_pins_and_os_filters_narrow_the_pick() {
        let none = anthropic();
        let mut mac = machine("mac", false);
        mac.announcement.os = "macos".into();
        let mut idle_b = machine("b", false);
        cap(&mut idle_b).load_1m = 0.0;
        let all = [machine("here", true), idle_b, mac];
        let pin = HostPin::Machine("mac".into());
        assert_eq!(choose(&all, &ask(&none, Some(&pin))).unwrap().picks, [Pick::There(2)], "a pin by name has no fallback");
        assert_eq!(choose(&all, &ask(&none, Some(&HostPin::Any))).unwrap().picks[0], Pick::There(1), "any: the most room, here no more preferred");
        let macos = HostPin::Os("macos".into());
        assert_eq!(choose(&all, &ask(&none, Some(&macos))).unwrap().picks, [Pick::There(2)]);
        let windows = HostPin::Os("windows".into());
        assert!(choose(&all, &ask(&none, Some(&windows))).is_err(), "no windows machine");
        let gone = HostPin::Machine("nowhere".into());
        assert!(choose(&all, &ask(&none, Some(&gone))).is_err());
    }

    // The Placer, over a fake fleet: the registry and the machines are
    // scripted, so what is under test is the placing and the following.

    use krowk_client::protocol::ErrorInfo;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHILD: &str = "0190f3c2-0000-7000-8000-0000000000c1";

    #[derive(Default)]
    struct Fake {
        here: Option<Candidate>,
        others: Vec<Candidate>,
        /// Each machine's answers to its `run`s, in turn.
        answers: Mutex<HashMap<String, VecDeque<Option<HostAnswer>>>>,
        sent: Mutex<Vec<(String, RunTask)>>,
        events: Mutex<Vec<Value>>,
        seen: Mutex<Option<Duration>>,
        others_read: AtomicUsize,
        landed: Mutex<Vec<String>>,
        spent: AtomicUsize,
        opened: AtomicUsize,
        closed: AtomicUsize,
        too_big: bool,
        stopped: Mutex<Vec<String>>,
        /// How long `send_run` takes to answer.
        slow: Duration,
    }

    struct Shared(Arc<Fake>);

    impl Fleet for Shared {
        fn device(&self) -> String {
            "here-id".into()
        }
        fn resolve_model(&self, asked: &ModelChoice) -> Result<ModelRef, String> {
            Ok(ModelRef { instance: asked.instance.clone().unwrap_or("anthropic".into()), model: asked.model.clone().unwrap_or("claude-opus-5-5".into()) })
        }
        fn here(&self, _: &ModelRef) -> Candidate {
            self.0.here.clone().unwrap()
        }
        fn others(&self) -> Result<Vec<Candidate>, String> {
            self.0.others_read.fetch_add(1, Ordering::SeqCst);
            Ok(self.0.others.clone())
        }
        fn repository(&self, _: &std::path::Path) -> Option<(String, String)> {
            Some((ORIGIN.into(), "https://github.com/acme/app.git".into()))
        }
        fn open_store(&self, given: Option<&JobStore>) -> Result<JobStore, String> {
            self.0.opened.fetch_add(1, Ordering::SeqCst);
            Ok(given.cloned().unwrap_or(JobStore { session: "job-session".into(), lease_token: "t".into() }))
        }
        fn close_store(&self, _: &JobStore) {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
        fn pack_job(&self, _: &std::path::Path, session: &str, prompt: &str) -> Result<super::super::handoff::Bundle, String> {
            if self.0.too_big {
                return Err(super::super::handoff::JOB_TOO_BIG.into());
            }
            Ok(super::super::handoff::Bundle { v: 1, session: session.into(), base: "abc".into(), subdir: String::new(), diff: Vec::new(), untracked: Vec::new(), transcript: None, commits: Vec::new(), prompt: prompt.into() })
        }
        fn put_job(&self, store: &JobStore, _: &super::super::handoff::Bundle) -> Result<JobRef, String> {
            Ok(JobRef { session: store.session.clone(), transport: "slug".into(), key: "k".repeat(64) })
        }
        fn spend_job(&self, _: &JobRef) {
            self.0.spent.fetch_add(1, Ordering::SeqCst);
        }
        fn stop(&self, child: &str) -> Result<(), String> {
            self.0.stopped.lock().unwrap().push(child.to_string());
            Ok(())
        }
        fn send_run(&self, to: &Announcement, task: &RunTask, _: Duration) -> Result<Option<HostAnswer>, String> {
            std::thread::sleep(self.0.slow);
            self.0.sent.lock().unwrap().push((to.name.clone(), task.clone()));
            self.0.answers.lock().unwrap().get_mut(&to.name).and_then(|a| a.pop_front()).ok_or_else(|| "no answer scripted".to_string())
        }
        fn watch(&self, _: &str) -> Result<Vec<Value>, String> {
            Ok(self.0.events.lock().unwrap().clone())
        }
        fn seen(&self, _: &str) -> Option<Duration> {
            *self.0.seen.lock().unwrap()
        }
        fn read_result(&self, child: &str, _: &str, _: &str) -> Result<super::super::handoff::Bundle, String> {
            Ok(super::super::handoff::Bundle { v: 1, session: child.into(), base: "snap".into(), subdir: String::new(), diff: Vec::new(), untracked: Vec::new(), transcript: None, commits: Vec::new(), prompt: String::new() })
        }
        fn land(&self, _: &super::super::handoff::Bundle, result: &super::super::handoff::Bundle, to: &std::path::Path) -> Result<(crate::worktree::Worktree, crate::worktree::apply::Applied), String> {
            self.0.landed.lock().unwrap().push(format!("{} -> {}", result.session, to.display()));
            let wt = crate::worktree::Worktree { path: "/wt".into(), hex: "ab12cd34".into(), base: "snap".into(), common: "/c".into(), main: "/m".into(), repo_id: "r".into() };
            Ok((wt, crate::worktree::apply::Applied::Applied(vec!["fixed.rs".into()])))
        }
    }

    fn event(body: LogBody) -> Value {
        serde_json::to_value(LogEvent { id: krowk_store::new_id(), parent_id: None, session_id: CHILD.into(), time_ms: 0, body }).unwrap()
    }

    /// What a child's log holds when its one turn completed and its result
    /// was stored.
    fn completed_child() -> Vec<Value> {
        vec![
            event(LogBody::ItemCompleted { turn_id: "t".into(), item_id: "i".into(), item: Item::AssistantText { text: "added fixed.rs".into() } }),
            event(LogBody::TurnCompleted { turn_id: "t".into(), status: TurnStatus::Completed, usage: Usage { output_tokens: 7, ..Usage::default() }, duration_ms: 1, reported_cost_usd: None, reported_tokens: None, error: None }),
            event(LogBody::TaskResult { turn_id: "t".into(), transport: Some("art_rslug".into()), key: Some("ab".repeat(32)), against: "snap".into(), denied: vec!["bash: rm -rf".into()] }),
        ]
    }

    fn started() -> Option<HostAnswer> {
        Some(HostAnswer::Started { session_id: CHILD.into() })
    }

    fn refused(code: &str) -> Option<HostAnswer> {
        Some(HostAnswer::refused(code, format!("{code} said so")))
    }

    fn quick() -> Timing {
        Timing { recent: Duration::from_secs(30), answer_wait: Duration::from_secs(1), answer_grace: Duration::from_secs(1), offline_after: Duration::from_millis(150), poll: Duration::from_millis(5) }
    }

    fn spec() -> TaskSpec {
        TaskSpec { description: "fix it".into(), prompt: "fix the test".into(), cwd: "/work/app".into(), host: None, model: ModelChoice::default(), budget: BudgetLimits::default(), permission_mode: None, store: None }
    }

    /// A fleet of `here` busy (all build slots in use) and the machines
    /// `others`, each answering as scripted.
    fn fleet(busy_here: bool, others: &[&str], answers: &[(&str, Vec<Option<HostAnswer>>)]) -> Arc<Fake> {
        let mut here = machine("here", true);
        if busy_here {
            cap(&mut here).build_slots.in_use = 4;
        }
        let others = others.iter().map(|n| machine(n, false)).collect();
        let answers = answers.iter().map(|(n, a)| (n.to_string(), a.iter().cloned().collect())).collect();
        Arc::new(Fake { here: Some(here), others, answers: Mutex::new(answers), ..Fake::default() })
    }

    fn placer(f: &Arc<Fake>) -> Arc<Placer> {
        Placer::with(Box::new(Shared(f.clone())), quick())
    }

    /// D4: with room here, the task stays and the registry is never read.
    #[test]
    fn a_task_with_room_here_stays_here_and_reads_nothing() {
        let f = fleet(false, &["b"], &[]);
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        assert_eq!(p.wait(&h, Duration::from_secs(5)), Ok(Progress::Here { why: "this machine has room".into() }));
        assert_eq!(f.others_read.load(Ordering::SeqCst), 0);
        assert_eq!(stream_line(&Progress::Here { why: "this machine has room".into() }).unwrap(), "Running here: this machine has room.");
    }

    /// R-ALLOC-7, 8, 14: with here busy the job goes to the other machine,
    /// the child is followed to its result, which lands in the working
    /// tree, and the outcome says what was applied, denied and spent.
    #[test]
    fn a_busy_machine_hands_the_task_to_another_and_its_result_lands_here() {
        let f = fleet(true, &["b"], &[("b", vec![started()])]);
        *f.events.lock().unwrap() = completed_child();
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        let running = p.placed(&h, Duration::from_secs(5)).unwrap();
        assert_eq!(stream_line(&running).unwrap(), "Running on b: this machine is busy (4 of 4 build slots in use).");
        let Ok(Progress::Done(out)) = p.wait(&h, Duration::from_secs(5)) else { panic!("not done") };
        assert_eq!((out.machine.as_str(), out.summary.as_str(), out.applied.as_slice(), out.kept.as_deref()), ("b", "added fixed.rs", &["fixed.rs".to_string()][..], None));
        assert_eq!((out.usage.output_tokens, out.denied), (7, vec!["bash: rm -rf".to_string()]));
        assert_eq!(f.landed.lock().unwrap().as_slice(), [format!("{CHILD} -> /work/app")]);
        let sent = f.sent.lock().unwrap();
        let run = &sent[0].1;
        assert_eq!((run.origin.as_str(), run.requested_by.as_str(), run.job.session.as_str()), (ORIGIN, "here-id", "job-session"));
        assert!(run.not_after_ms > 0);
        assert_eq!((f.spent.load(Ordering::SeqCst), f.closed.load(Ordering::SeqCst)), (2, 1), "the job spent once a child started and the result once read, its own session let go");
    }

    /// D11: a refusal moves on to the next candidate once; the one after
    /// is never asked.
    #[test]
    fn a_refusal_moves_on_to_the_next_candidate_once() {
        let f = fleet(true, &["b", "c", "d"], &[("b", vec![refused("no_slot")]), ("c", vec![started()]), ("d", vec![started()])]);
        *f.events.lock().unwrap() = completed_child();
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        let Ok(Progress::Running { machine, why, .. }) = p.placed(&h, Duration::from_secs(5)) else { panic!("not running") };
        assert_eq!(machine, "c");
        assert!(why.contains("b would not take it"), "{why}");
        assert!(matches!(p.wait(&h, Duration::from_secs(5)), Ok(Progress::Done(_))));
        let asked: Vec<String> = f.sent.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(asked, ["b", "c"]);
    }

    /// D11: two refusals, and the task runs here, busy or not.
    #[test]
    fn two_refusals_and_the_task_runs_here() {
        let f = fleet(true, &["b", "c"], &[("b", vec![refused("not_accepting")]), ("c", vec![None])]);
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        let Ok(Progress::Here { why }) = p.wait(&h, Duration::from_secs(5)) else { panic!("not here") };
        assert!(why.contains("c would not take it"), "{why}");
        assert_eq!(f.closed.load(Ordering::SeqCst), 1);
        assert_eq!(f.spent.load(Ordering::SeqCst), 0, "the job is not spent: no child ran");
    }

    /// D15: every candidate left over the machine limit points to billing.
    #[test]
    fn every_candidate_over_the_machine_limit_fails_pointing_to_billing() {
        let mut mac_a = machine("a", false);
        mac_a.announcement.os = "macos".into();
        let mut mac_b = machine("b", false);
        mac_b.announcement.os = "macos".into();
        let f = Arc::new(Fake {
            here: Some(machine("here", true)),
            others: vec![mac_a, mac_b],
            answers: Mutex::new(HashMap::from([("a".to_string(), VecDeque::from([refused("machine_limit")])), ("b".to_string(), VecDeque::from([refused("machine_limit")]))])),
            ..Fake::default()
        });
        let p = placer(&f);
        let mut s = spec();
        s.host = Some(HostPin::Os("macos".into()));
        let h = p.start(s, Here::Caller);
        let got = p.wait(&h, Duration::from_secs(5)).unwrap_err();
        assert_eq!(got, Failure::MachineLimit);
        assert!(got.to_string().contains("krowk billing"));
    }

    /// R-ALLOC-9: a machine that stops announcing and streaming fails the
    /// task naming it.
    #[test]
    fn a_machine_that_goes_quiet_fails_the_task_naming_it() {
        let f = fleet(true, &["b"], &[("b", vec![started()])]);
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        let got = p.wait(&h, Duration::from_secs(5)).unwrap_err();
        assert_eq!(got, Failure::Offline { machine: "b".into() });
        assert!(got.to_string().contains("work is in a worktree there"));
    }

    /// D11: after the child started, its failure comes back as the result.
    #[test]
    fn a_child_that_fails_after_it_started_is_the_tool_result() {
        let f = fleet(true, &["b"], &[("b", vec![started()])]);
        *f.events.lock().unwrap() = vec![event(LogBody::TurnCompleted { turn_id: "t".into(), status: TurnStatus::Failed, usage: Usage::default(), duration_ms: 1, reported_cost_usd: None, reported_tokens: None, error: Some(ErrorInfo { code: "x".into(), message: "the model quit".into(), http_status: None, resets_at_ms: None }) })];
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        assert_eq!(p.wait(&h, Duration::from_secs(5)).unwrap_err(), Failure::Failed { machine: "b".into(), reason: "the model quit".into() });
        assert_eq!(f.sent.lock().unwrap().len(), 1, "never retried");
    }

    /// D9: a placement sent counts as an agent slot in use for 30 seconds,
    /// so a second task goes to another machine when the first has one slot.
    #[test]
    fn a_placement_just_made_counts_as_a_slot_in_use() {
        let mut b = machine("b", false);
        cap(&mut b).agent_slots = Slots { in_use: 0, total: 1 };
        let mut c = machine("c", false);
        cap(&mut c).agent_slots = Slots { in_use: 0, total: 1 };
        let f = Arc::new(Fake { here: Some({ let mut h = machine("here", true); cap(&mut h).build_slots.in_use = 4; h }), others: vec![b, c], answers: Mutex::new(HashMap::from([("b".to_string(), VecDeque::from([started()])), ("c".to_string(), VecDeque::from([started()]))])), ..Fake::default() });
        *f.events.lock().unwrap() = completed_child();
        let p = placer(&f);
        let h1 = p.start(spec(), Here::Caller);
        assert!(matches!(p.placed(&h1, Duration::from_secs(5)), Ok(Progress::Running { .. })));
        let h2 = p.start(spec(), Here::Caller);
        let Ok(Progress::Running { machine, .. }) = p.placed(&h2, Duration::from_secs(5)) else { panic!("not running") };
        assert_eq!(f.sent.lock().unwrap().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(machine, "c");
    }

    /// D11: a `started` that comes after placement moved on has its child
    /// stopped, and the next pick is tried.
    #[test]
    fn a_child_started_too_late_is_stopped_and_the_next_pick_tried() {
        let mut f = fleet(true, &["b", "c"], &[("b", vec![started()]), ("c", vec![started()])]);
        let slow = Arc::get_mut(&mut f).unwrap();
        slow.slow = Duration::from_millis(60);
        let p = Placer::with(Box::new(Shared(f.clone())), Timing { answer_wait: Duration::from_millis(5), answer_grace: Duration::from_millis(5), ..quick() });
        *f.events.lock().unwrap() = completed_child();
        let h = p.start(spec(), Here::Caller);
        let got = p.wait(&h, Duration::from_secs(5));
        // Both answers came late: both children stopped, then here.
        assert!(matches!(got, Ok(Progress::Here { .. })), "{got:?}");
        assert_eq!(f.stopped.lock().unwrap().len(), 2);
        assert_eq!(f.spent.load(Ordering::SeqCst), 0);
    }

    /// A job past the bound fails with the fix; a pin to a machine that
    /// refuses has no fallback; an id never started is unknown.
    #[test]
    fn too_big_a_pin_without_fallback_and_an_unknown_task() {
        let mut big = fleet(true, &["b"], &[]);
        Arc::get_mut(&mut big).unwrap().too_big = true;
        let p = placer(&big);
        let h = p.start(spec(), Here::Caller);
        assert_eq!(p.wait(&h, Duration::from_secs(5)).unwrap_err(), Failure::TooBig);

        let f = fleet(false, &["b", "c"], &[("b", vec![refused("no_clone")])]);
        let p = placer(&f);
        let mut s = spec();
        s.host = Some(HostPin::Machine("b".into()));
        let h = p.start(s, Here::Caller);
        let Err(Failure::NoMachine(why)) = p.wait(&h, Duration::from_secs(5)) else { panic!("placed") };
        assert!(why.contains("b: no_clone said so"), "{why}");
        assert_eq!(f.sent.lock().unwrap().len(), 1);

        assert_eq!(p.wait(&Handle { id: "nope".into(), cwd: "/".into() }, Duration::ZERO), Err(Failure::Unknown("nope".into())));
    }

    /// `Here::Worktree` with nowhere to run a child says so.
    #[test]
    fn a_local_worktree_task_with_no_runner_fails_plainly() {
        let f = fleet(false, &[], &[]);
        let p = placer(&f);
        let h = p.start(spec(), Here::Worktree);
        let Err(Failure::Failed { machine, reason }) = p.wait(&h, Duration::from_secs(5)) else { panic!("ran") };
        assert_eq!(machine, "here");
        assert!(reason.contains("no way to run"), "{reason}");
    }

    /// A child whose turn ended and whose bridge never logged a result
    /// fails the task rather than waiting for ever.
    #[test]
    fn a_turn_that_ends_with_no_result_fails_the_task() {
        let f = fleet(true, &["b"], &[("b", vec![started()])]);
        *f.events.lock().unwrap() = completed_child()[..2].to_vec();
        *f.seen.lock().unwrap() = Some(Duration::ZERO);
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        let Err(Failure::Failed { machine, reason }) = p.wait(&h, Duration::from_secs(5)) else { panic!("finished") };
        assert_eq!(machine, "b");
        assert!(reason.contains("no result"), "{reason}");
    }

    /// An interrupted caller stops the child and lands nothing.
    #[test]
    fn a_cancelled_task_stops_its_child_and_lands_nothing() {
        let f = fleet(true, &["b"], &[("b", vec![started()])]);
        *f.seen.lock().unwrap() = Some(Duration::ZERO);
        let p = placer(&f);
        let h = p.start(spec(), Here::Caller);
        assert!(matches!(p.placed(&h, Duration::from_secs(5)), Ok(Progress::Running { .. })));
        p.cancel(&h);
        *f.events.lock().unwrap() = completed_child();
        let got = p.wait(&h, Duration::from_secs(5));
        assert!(matches!(got, Err(Failure::Failed { .. })), "{got:?}");
        assert_eq!(f.stopped.lock().unwrap().as_slice(), [CHILD]);
        assert!(f.landed.lock().unwrap().is_empty());
    }
}
