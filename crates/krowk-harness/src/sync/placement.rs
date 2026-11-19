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
use krowk_client::host_command::ModelChoice;
use krowk_client::presence::{Announcement, Capacity};
use krowk_client::protocol::{BudgetLimits, LogEvent, ModelRef, PermissionMode, Usage};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
/// use"; none when it has room. Slice 07 builds it.
pub fn busy(c: &Capacity) -> Option<String> {
    todo!("slice 07: whether {c:?} is busy")
}

/// Why `c` cannot take `ask` at all (see the module's docs), with the fix
/// where there is one ("run: git clone <clone_url>"); none when it can.
/// Slice 07 builds it.
pub fn ruled_out(c: &Candidate, ask: &Ask<'_>) -> Option<String> {
    todo!("slice 07: whether {} can take {}", c.announcement.name, ask.origin)
}

/// The picks among `candidates` for `ask` (see the module's docs); `Err`
/// is why none can take it, each machine's reason and fix. Slice 07 builds
/// it.
pub fn choose(candidates: &[Candidate], ask: &Ask<'_>) -> Result<Choice, String> {
    todo!("slice 07: choose among {} for {}", candidates.len(), ask.origin)
}

/// Whether a `started` received at `received_ms` (the supervisor's clock)
/// for a `run` whose `notAfterMs` was `not_after_ms` came in time: by
/// `notAfterMs` and `ANSWER_GRACE` more. One that did not has its child
/// stopped (see the module's docs). Slice 07 builds it.
pub fn in_time(not_after_ms: i64, received_ms: i64) -> bool {
    todo!("slice 07: whether {received_ms} is in time for {not_after_ms}")
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
/// which any of the person's devices can write. Slice 07 builds it.
pub fn result_of(events: &[LogEvent], child: &str) -> Result<Option<ResultRef>, String> {
    todo!("slice 07: the result of {child} in {} events", events.len())
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

/// The supervisor's side: places tasks and brings their results back. One
/// per supervisor, kept for its life, so its placements of the last
/// `RECENT` count (D9) and its tasks can be collected. Blocking: an async
/// caller waits on a blocking thread. Slice 07 adds what it keeps of its
/// tasks.
pub struct Placer {
    pub announcer: Announcer,
}

impl Placer {
    pub fn new(announcer: Announcer) -> Arc<Placer> {
        Arc::new(Placer { announcer })
    }

    /// `spec` handed over: its `Handle` at once, the placing done on a
    /// thread of its own, `here` running a pick of this machine. Slice 07
    /// builds it.
    pub fn start(self: &Arc<Self>, spec: TaskSpec, here: Here) -> Handle {
        todo!("slice 07: place {} ({here:?})", spec.description)
    }

    /// Up to `most` for the task to move past placing and running: where
    /// it is then, its result landed in `h.cwd` once done, taken only as
    /// `result_of` checks it. Slice 07 builds it.
    pub fn wait(&self, h: &Handle, most: Duration) -> Result<Progress, Failure> {
        todo!("slice 07: wait {most:?} for {}", h.id)
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
    #[ignore = "slice 07: busy"]
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
    #[ignore = "slice 07: ruled_out"]
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
    #[ignore = "slice 07: choose"]
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
    #[ignore = "slice 07: in_time"]
    fn alloc_07_a_started_after_the_grace_is_late() {
        let grace = ANSWER_GRACE.as_millis() as i64;
        assert!(in_time(30_000, 29_000));
        assert!(in_time(30_000, 30_000 + grace));
        assert!(!in_time(30_000, 30_001 + grace));
    }

    /// Slice 07 (R-ALLOC-8): a result is taken only from the started
    /// child's own log, after its turn completed, in shape.
    #[test]
    #[ignore = "slice 07: result_of"]
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
        let other_turn = ev(CHILD, LogBody::TaskResult { turn_id: "u".into(), transport: Some("art_1".into()), key: key(), against: "abc".into(), denied: Vec::new() });
        assert!(result_of(&[done(), other_turn], CHILD).is_err(), "for another turn");
        let first = result_of(&[done(), result(CHILD, Some("art_1"), key()), result(CHILD, Some("art_2"), key())], CHILD).unwrap().unwrap();
        assert_eq!(first.transport.as_deref(), Some("art_1"), "the first matching one");
        assert!(result_of(&[done(), result(CHILD, Some("art_1"), None)], CHILD).is_err(), "a transport with no key");
    }

    /// Slice 07 (D9): this supervisor's recent placements fill a machine's
    /// agent slots, added without overflowing.
    #[test]
    #[ignore = "slice 07: ruled_out"]
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
    #[ignore = "slice 07: choose with a host pin"]
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
}
