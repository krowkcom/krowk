//! Placement (R-ALLOC-7 … R-ALLOC-11; engineering/harness.md → Sync →
//! Automatic allocation): which of the person's machines runs a task a
//! supervisor hands over, and the task's way there and back. krowk's own
//! `subagent` tool and krowk-mcp's `krowk_delegate` and `krowk_collect` are
//! its two callers, and share all of it.
//!
//! **It runs on the supervisor's machine** (D3), from the announcements
//! opened there (`krowk_client::presence`): the registry sees only sealed
//! bytes, so it cannot place anything.
//!
//! **The pick** (`choose`, pure): every machine not `ruled_out` (D6, D7,
//! D14: offline, not accepting, no clone of the repository's origin, no
//! ready instance for the model, no free agent slot, counting this
//! supervisor's own placements on it of the last `RECENT` as slots in use,
//! D9), then this machine unless it is `busy` (D4, D5), else the others
//! ranked by free agent slots, then free build slots, then lowest load per
//! core, then most free memory, a machine whose instance for the model is
//! near its usage limit after those whose is not. A `HostPin` narrows it:
//! a machine by name, `any` (the best, this machine no more preferred than
//! another), or `any:<os>`. The pick, then one more to fall back on.
//!
//! **The way there** (`Placer::start`): the job packed
//! (`handoff::pack_job`) and stored as a transport, `run` sent
//! (`host_command::RunTask`); a refusal, or no answer within
//! `ANSWER_WAIT`, moves to the fallback once (D11): nothing ran yet. A
//! `Placed::Here` is this machine's to run as it runs any subagent.
//!
//! **The way back** (`Placer::wait`): the child is followed as any synced
//! session is; when its turn completes, the result its log names
//! (`handoff::ResultNote`) is fetched and landed in the supervisor's working
//! tree (`handoff::apply_result`). A failure after the child started is
//! the caller's, never retried (D11): `Failure`. A machine with no
//! announcement for `OFFLINE_AFTER` and no stream has gone (R-ALLOC-9).
//!
//! Slice 07 builds `busy`, `ruled_out`, `choose` and the `Placer`; slice 08
//! calls it from krowk-mcp.

use super::announce::Announcer;
use krowk_client::host_command::ModelChoice;
use krowk_client::presence::{Announcement, Capacity};
use krowk_client::protocol::{BudgetLimits, Usage};
use std::path::PathBuf;
use std::time::Duration;

/// How long a placement this supervisor made counts as an agent slot in
/// use on its machine (D9), so calls made together spread out.
pub const RECENT: Duration = Duration::from_secs(30);
/// How long a `run` waits for its answer before the machine counts as
/// silent and the next is tried (D11): the host-silence limit `krowk sync
/// new --host` waits.
pub const ANSWER_WAIT: Duration = Duration::from_secs(30);
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
    pub model: ModelChoice,
    /// What is left of the supervisor's tree budget (D12).
    pub budget: BudgetLimits,
}

/// One machine as placement sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Its announcement, opened.
    pub announcement: Announcement,
    /// The registry lists it online.
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
    /// Its origin as the supervisor's clone has it, which a fix names:
    /// `git clone <clone_url>`.
    pub clone_url: &'a str,
    pub model: &'a ModelChoice,
    pub host: Option<&'a HostPin>,
}

/// One machine picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// This machine: the caller runs it as it runs any subagent.
    Here,
    /// The candidate at this index, sent a `run`.
    There(usize),
}

/// The pick, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The pick first, then at most one to fall back on (D11).
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

/// Why `c` cannot take `ask` at all (D6, D7, D14), with the fix where
/// there is one ("run: git clone <clone_url>"); none when it can. Slice 07
/// builds it.
pub fn ruled_out(c: &Candidate, ask: &Ask<'_>) -> Option<String> {
    todo!("slice 07: whether {} can take {}", c.announcement.name, ask.origin)
}

/// The pick among `candidates` for `ask` (see the module's docs); `Err` is
/// why none can take it, each machine's reason and fix. Slice 07 builds it.
pub fn choose(candidates: &[Candidate], ask: &Ask<'_>) -> Result<Choice, String> {
    todo!("slice 07: choose among {} for {}", candidates.len(), ask.origin)
}

/// A task placed.
#[derive(Debug)]
pub enum Placed {
    /// On this machine: the caller runs it.
    Here { why: String },
    /// Sent to another, which started it.
    There(Handle),
}

/// A task running on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// The task's id, krowk_collect's argument: the child's session id.
    pub id: String,
    /// The machine's name, and its device id in hex.
    pub machine: String,
    pub device: String,
    /// Why it runs there, as the parent's stream says it.
    pub why: String,
    /// The working tree the result lands in.
    pub cwd: PathBuf,
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
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoMachine(why) => write!(f, "{why}"),
            Failure::MachineLimit => write!(f, "No machine could take it without going over your plan's machine limit. See krowk billing."),
            Failure::TooBig => write!(f, "{}", super::handoff::JOB_TOO_BIG),
            Failure::Offline { machine } => write!(f, "{machine} went offline mid-task; its work is in a worktree there."),
            Failure::Failed { machine, reason } => write!(f, "The task failed on {machine}: {reason}"),
        }
    }
}

/// The supervisor's side: places tasks and brings their results back. One
/// per supervisor, kept for its life, so its placements of the last
/// `RECENT` count (D9). Blocking: an async caller runs it on a blocking
/// thread.
pub struct Placer {
    pub announcer: Announcer,
}

impl Placer {
    pub fn new(announcer: Announcer) -> Placer {
        Placer { announcer }
    }

    /// `spec` placed, and started there unless here (see the module's
    /// docs). Slice 07 builds it.
    pub fn start(&self, spec: TaskSpec) -> Result<Placed, Failure> {
        todo!("slice 07: place {}", spec.description)
    }

    /// `spec` sent to this machine's own host daemon as a `run`, for a
    /// caller with no engine of its own to run a `Placed::Here` (krowk-mcp).
    /// Slice 07 builds it.
    pub fn start_here(&self, spec: TaskSpec) -> Result<Handle, Failure> {
        todo!("slice 07: run {} on this machine's daemon", spec.description)
    }

    /// Up to `most` for `h` to finish: its outcome, its result landed in
    /// `h.cwd`, or none while it still runs. Slice 07 builds it.
    pub fn wait(&self, h: &Handle, most: Duration) -> Result<Option<Outcome>, Failure> {
        todo!("slice 07: wait {most:?} for {}", h.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krowk_client::presence::{AnnouncedInstance, AnnouncedRepository, Slots};
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

    fn ask<'a>(model: &'a ModelChoice, host: Option<&'a HostPin>) -> Ask<'a> {
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
        let none = ModelChoice::default();
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
        let codex = ModelChoice { instance: Some("codex".into()), model: None };
        assert!(ruled_out(&machine("m", false), &ask(&codex, None)).is_some(), "no ready instance for the model");
        let mut m = machine("m", false);
        cap(&mut m).agent_slots.in_use = 7;
        m.placed_recently = 1;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "its last free agent slot is one this supervisor just placed on");
        let mut m = machine("m", false);
        m.announcement.capacity = None;
        assert!(ruled_out(&m, &ask(&none, None)).is_some(), "a host older than allocation");
    }

    /// Slice 07 (D4, D5, D6): here unless here is busy; then free agent
    /// slots, free build slots, load per core, free memory, and a machine
    /// near its usage limit last; the pick and one fallback.
    #[test]
    #[ignore = "slice 07: choose"]
    fn alloc_07_work_stays_here_unless_busy_then_goes_to_the_most_room() {
        let none = ModelChoice::default();
        let (here, b, c) = (machine("here", true), machine("b", false), machine("c", false));
        let roomy = choose(&[here.clone(), b.clone(), c.clone()], &ask(&none, None)).unwrap();
        assert_eq!((roomy.picks[0], roomy.why.as_str()), (Pick::Here, "this machine has room"));

        let mut busy_here = here.clone();
        cap(&mut busy_here).build_slots.in_use = 4;
        let mut fewer_agents = b.clone();
        cap(&mut fewer_agents).agent_slots.in_use = 3;
        let got = choose(&[busy_here.clone(), fewer_agents.clone(), c.clone()], &ask(&none, None)).unwrap();
        assert_eq!(got.picks, [Pick::There(2), Pick::There(1)], "most free agent slots first, one fallback");
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

        assert_eq!(choose(&[busy_here], &ask(&none, None)).unwrap().picks[0], Pick::Here, "busy, but no other machine can take it: here");
    }

    /// Slice 07 (R-ALLOC-10): a pin to a machine by name, `any`, and an OS
    /// filter.
    #[test]
    #[ignore = "slice 07: choose with a host pin"]
    fn alloc_07_host_pins_and_os_filters_narrow_the_pick() {
        let none = ModelChoice::default();
        let mut mac = machine("mac", false);
        mac.announcement.os = "macos".into();
        let mut idle_b = machine("b", false);
        cap(&mut idle_b).load_1m = 0.0;
        let all = [machine("here", true), idle_b, mac];
        let pin = HostPin::Machine("mac".into());
        assert_eq!(choose(&all, &ask(&none, Some(&pin))).unwrap().picks[0], Pick::There(2));
        assert_eq!(choose(&all, &ask(&none, Some(&HostPin::Any))).unwrap().picks[0], Pick::There(1), "any: the most room, here no more preferred");
        let macos = HostPin::Os("macos".into());
        assert_eq!(choose(&all, &ask(&none, Some(&macos))).unwrap().picks, [Pick::There(2)]);
        let windows = HostPin::Os("windows".into());
        assert!(choose(&all, &ask(&none, Some(&windows))).is_err(), "no windows machine");
        let gone = HostPin::Machine("nowhere".into());
        assert!(choose(&all, &ask(&none, Some(&gone))).is_err());
    }
}
