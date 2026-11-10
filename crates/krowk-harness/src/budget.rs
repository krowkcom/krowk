//! The budget (R-BUDGET-1): what a session may spend, checked by the engine
//! before every model call it makes, so a runaway stops before the call
//! that would cross the limit rather than after it.
//!
//! What counts is what the provider metered — the `usage` of every
//! `response.completed` — never what a request asked for: a request's
//! `max_tokens` is a wish, and providers overshoot it (two qwen3.5-plus
//! calls sent with 1,200 completed 1,379 and 3,422). It is counted over
//! the session and every session descending from it by `parentSessionId`
//! (subagents, and theirs), on every branch of each log: spend on a branch
//! a rewind abandoned was still spent. `--max-tokens` counts generated
//! tokens — output and reasoning, the part a cap is meant to bound — and
//! `--max-usd` the whole cost, priced from models.dev at current rates the
//! way every krowk figure is. Both are the limits `krowk sessions budget`
//! takes, with its meaning: at the limit is inside it.
//!
//! The next call's cost is not known before it is made, so the check asks
//! what it costs at least. It resends at least the whole prompt of the call
//! before it on the same model — the log is append-only, so the
//! conversation only grows — and generates at least one token; priced as if
//! that prompt were all read from cache, the cheapest a provider charges
//! for input, that is a floor no real call comes in under. A call is
//! refused when what has been spent plus that floor would go past the
//! limit. A model with no price trips `--max-usd` outright: an unknown
//! spend is not a spend inside the limit.
//!
//! A backend makes its own model calls, so for one the host checks before
//! the turn starts and interrupts the turn as soon as a metered call has
//! gone over (`Budget::over`).

use crate::engine::{BoxFuture, EngineError};
use crate::host::Pricer;
use crate::log;
use crate::protocol::{BudgetLimits, LogBody, LogEvent, Usage};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// The code a refused call ends its turn with: `krowk sessions budget`'s,
/// so a script branches on one code and one exit status (4) for both.
pub const EXCEEDED: &str = "budget_exceeded";

/// Metered usage, and what it cost.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spend {
    pub usage: Usage,
    /// The priced part, in USD, unrounded from the call to the comparison.
    pub known_usd: f64,
    /// `provider/model` of every call with no price.
    pub unpriced: BTreeSet<String>,
}

impl Spend {
    /// Output and reasoning: what `--max-tokens` holds to its limit.
    pub fn generated(&self) -> i64 {
        self.usage.output_tokens.max(0) + self.usage.reasoning_tokens.max(0)
    }

    /// The whole cost, or none when any of it has no price.
    pub fn cost(&self) -> Option<f64> {
        self.unpriced.is_empty().then_some(self.known_usd)
    }

    /// One call, priced by the model the request named, else the one the
    /// provider says answered (a snapshot id) — as the host prices a turn.
    pub fn add(&mut self, pricer: &Pricer, provider: &str, asked: &str, answered: &str, usage: &Usage) {
        self.usage += *usage;
        match pricer(provider, asked, usage).or_else(|| pricer(provider, answered, usage)) {
            Some(usd) => self.known_usd += usd,
            None => {
                self.unpriced.insert(format!("{provider}/{asked}"));
            }
        }
    }

    pub fn merge(&mut self, o: &Spend) {
        self.usage += o.usage;
        self.known_usd += o.known_usd;
        self.unpriced.extend(o.unpriced.iter().cloned());
    }
}

/// The last metered call: what the next one's floor is worked out from.
#[derive(Debug, Clone, PartialEq)]
struct LastCall {
    provider: String,
    /// The id the request named — an alias, for a backend (`sonnet`).
    model: String,
    /// The id the provider says answered (`claude-sonnet-4-6-…`): what an
    /// alias is priced by.
    answered: String,
    usage: Usage,
}

/// A turn's spend once its backend has said what it cost: the larger of
/// the two, since a vendor's total counts what krowk never saw metered, and
/// a reported total prices what krowk could not.
fn reconcile(turn: &mut Spend, reported: f64) {
    if reported > turn.known_usd || !turn.unpriced.is_empty() {
        turn.known_usd = turn.known_usd.max(reported);
        turn.unpriced.clear();
    }
}

/// Every model call a log records — a backend's subagents' included —
/// priced, turn by turn, and the last of the session's own calls.
fn metered(events: &[LogEvent], pricer: &Pricer) -> (Spend, Option<LastCall>) {
    let mut m = LogMeter::default();
    for ev in events {
        m.apply(ev, pricer);
    }
    (m.spend(), m.last)
}

/// One log's spend, as far as it has been read: what each turn ran on, what
/// each has cost, and where the next read starts. A log only grows, so
/// once read a line is never read again.
#[derive(Default)]
struct LogMeter {
    offset: u64,
    turns: HashMap<String, (String, String)>,
    per_turn: HashMap<String, Spend>,
    last: Option<LastCall>,
}

impl LogMeter {
    fn apply(&mut self, ev: &LogEvent, pricer: &Pricer) {
        match &ev.body {
            LogBody::TurnStarted { turn_id, model, provider, .. } => {
                self.turns.insert(turn_id.clone(), (provider.clone(), model.model.clone()));
            }
            LogBody::ResponseCompleted { turn_id, model: answered, usage, .. } | LogBody::SubagentResponse { turn_id, model: answered, usage, .. } => {
                let (provider, asked) = self.turns.get(turn_id.as_str()).cloned().unwrap_or_else(|| (String::new(), answered.clone()));
                let spend = self.per_turn.entry(turn_id.clone()).or_default();
                if matches!(ev.body, LogBody::SubagentResponse { .. }) {
                    // A subagent runs on a model of its own (a sonnet
                    // session's haiku Task): priced by the one that answered.
                    spend.add(pricer, &provider, answered, &asked, usage);
                } else {
                    spend.add(pricer, &provider, &asked, answered, usage);
                    self.last = Some(LastCall { provider, model: asked, answered: answered.clone(), usage: *usage });
                }
            }
            LogBody::TurnCompleted { turn_id, reported_cost_usd: Some(r), .. } => reconcile(self.per_turn.entry(turn_id.clone()).or_default(), *r),
            _ => {}
        }
    }

    /// Reads what the log has gained since the last read. Lenient where the
    /// session log's reader is strict: a subagent's log can be mid-append,
    /// so only whole lines are read — a torn last one waits for the next
    /// read — and a line that does not parse costs that line, not the count.
    fn catch_up(&mut self, path: &Path, pricer: &Pricer) {
        use std::io::{Read, Seek};
        let Ok(mut f) = std::fs::File::open(path) else { return };
        if f.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut more = Vec::new();
        if f.read_to_end(&mut more).is_err() {
            return;
        }
        let Some(end) = more.iter().rposition(|b| *b == b'\n') else { return };
        for line in more[..=end].split(|b| *b == b'\n') {
            if let Ok(ev) = serde_json::from_slice::<LogEvent>(line) {
                self.apply(&ev, pricer);
            }
        }
        self.offset += end as u64 + 1;
    }

    fn spend(&self) -> Spend {
        let mut spend = Spend::default();
        for s in self.per_turn.values() {
            spend.merge(s);
        }
        spend
    }
}

/// The session a log's root names as its parent: `Some(None)` for a root
/// with none, and none while the root cannot be read yet.
fn parent_of(path: &Path) -> Option<Option<String>> {
    let f = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(f).read_line(&mut first).ok()?;
    match serde_json::from_str::<LogEvent>(&first).ok()?.body {
        LogBody::SessionStarted { parent_session_id, .. } => Some(parent_session_id),
        _ => Some(None),
    }
}

/// The spend of the tree under one session, counted as its logs grow: each
/// session's parent is read once (a root never changes), each log from
/// where the last count stopped. What a budget with a limit counts again
/// before every call, so that count costs what the logs gained, not their
/// length.
#[derive(Default)]
struct TreeMeter {
    parents: HashMap<String, Option<String>>,
    logs: HashMap<String, LogMeter>,
}

impl TreeMeter {
    fn spent(&mut self, sessions: &Path, root: &str, pricer: &Pricer) -> Spend {
        let newer: Vec<(String, PathBuf)> = log::list(sessions).unwrap_or_default().into_iter().filter(|(id, _)| id.as_str() > root).collect();
        for (id, path) in &newer {
            if !self.parents.contains_key(id)
                && let Some(p) = parent_of(path)
            {
                self.parents.insert(id.clone(), p);
            }
        }
        let mut tree: HashSet<String> = HashSet::from([root.to_string()]);
        // Until nothing joins: ids minted in one millisecond need not sort
        // in the order the sessions were made.
        loop {
            let before = tree.len();
            for (id, _) in &newer {
                if let Some(Some(p)) = self.parents.get(id)
                    && tree.contains(p)
                {
                    tree.insert(id.clone());
                }
            }
            if tree.len() == before {
                break;
            }
        }
        let mut total = Spend::default();
        for (id, path) in newer.iter().filter(|(id, _)| tree.contains(id)) {
            let m = self.logs.entry(id.clone()).or_default();
            m.catch_up(path, pricer);
            total.merge(&m.spend());
        }
        total
    }
}

/// Every session descending from `root` — its subagents, and theirs — by
/// the parent each one's root event names. A subagent is created after the
/// session that spawns it, and ids are UUIDv7s that sort by creation, so
/// only the logs newer than `root` are opened, and only their first line.
pub fn descendants(sessions: &Path, root: &str) -> Vec<(String, PathBuf)> {
    let newer: Vec<(String, PathBuf, String)> = log::list(sessions)
        .unwrap_or_default()
        .into_iter()
        .filter(|(id, _)| id.as_str() > root)
        .filter_map(|(id, path)| parent_of(&path).flatten().map(|p| (id, path, p)))
        .collect();
    let mut tree: HashSet<String> = HashSet::from([root.to_string()]);
    let mut out = Vec::new();
    // Until nothing joins: ids minted in one millisecond need not sort in
    // the order the sessions were made.
    loop {
        let before = out.len();
        for (id, path, parent) in &newer {
            if tree.contains(parent) && !tree.contains(id) {
                tree.insert(id.clone());
                out.push((id.clone(), path.clone()));
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// One limit the session would go past.
#[derive(Debug, Clone, PartialEq)]
pub enum Trip {
    /// `generated` tokens so far; `next` is whether a call is about to add
    /// at least one more.
    Tokens { generated: i64, limit: i64, next: bool },
    /// `spent` so far, and the least the next call costs when one is about
    /// to be made.
    Usd { spent: f64, next: Option<f64>, limit: f64 },
    /// Some of the cost has no price; `known` is the part that has one.
    Unknown { known: f64, unpriced: Vec<String> },
}

impl Trip {
    /// The sentence `krowk sessions budget` words the same trip in.
    fn sentence(&self) -> String {
        match self {
            Trip::Tokens { generated, limit, next: false } => format!("{generated} tokens generated, over --max-tokens {limit}"),
            Trip::Tokens { generated, limit, next: true } => {
                format!("{generated} tokens generated of --max-tokens {limit}, and the next model call generates at least one more")
            }
            Trip::Usd { spent, next: None, limit } => format!("{} metered, over --max-usd {}", usd(*spent), usd(*limit)),
            Trip::Usd { spent, next: Some(n), limit } => {
                format!("{} metered, and the next model call costs at least {}, over --max-usd {}", usd(*spent), usd(*n), usd(*limit))
            }
            Trip::Unknown { known, unpriced } => format!("the cost is unknown — at least {}, with no price for {}", usd(*known), unpriced.join(", ")),
        }
    }

    /// The flag a higher limit is passed with.
    fn flag(&self) -> &'static str {
        match self {
            Trip::Tokens { .. } => "--max-tokens",
            Trip::Usd { .. } | Trip::Unknown { .. } => "--max-usd",
        }
    }
}

/// Dollars as `krowk sessions budget` prints them: cents from a dollar up,
/// three significant figures below it, so a limit of a cent and a spend of
/// nine tenths of one never both read $0.01.
pub fn usd(v: f64) -> String {
    if v >= 1.0 || v <= 0.0 {
        return format!("${v:.2}");
    }
    let places = (2 - v.log10().floor() as i32).clamp(2, 12) as usize;
    format!("${v:.places$}")
}

/// Every limit `spent` is past, the first found. `next` is the floor of a
/// call about to be made — its usage, and its price when it has one — or
/// none for the check after a call. Strictly over trips; at the limit is
/// inside it.
pub fn judge(limits: &BudgetLimits, spent: &Spend, next: Option<(&Usage, Option<f64>, &str)>) -> Option<Trip> {
    if let Some(limit) = limits.max_tokens {
        let generated = spent.generated();
        let more = next.map_or(0, |(u, _, _)| u.output_tokens.max(0) + u.reasoning_tokens.max(0));
        if generated + more > limit {
            return Some(Trip::Tokens { generated, limit, next: next.is_some() });
        }
    }
    if let Some(limit) = limits.max_usd {
        let mut unpriced: Vec<String> = spent.unpriced.iter().cloned().collect();
        if let Some((_, None, model)) = next
            && !unpriced.iter().any(|m| m == model)
        {
            unpriced.push(model.to_string());
        }
        if !unpriced.is_empty() {
            return Some(Trip::Unknown { known: spent.known_usd, unpriced });
        }
        let floor = next.and_then(|(_, usd, _)| usd);
        if spent.known_usd + floor.unwrap_or(0.0) > limit {
            return Some(Trip::Usd { spent: spent.known_usd, next: floor, limit });
        }
    }
    None
}

/// The least a call on `model` costs, given the last metered call: it
/// resends at least that call's whole prompt and its answer, when it was on
/// the same model (another model's tokenizer counts differently), and
/// generates at least one token. The prompt is counted at the cache-read
/// rate, the cheapest input there is.
fn floor(last: Option<&LastCall>, provider: &str, model: &str) -> Usage {
    let resent = last
        .filter(|l| l.provider == provider && l.model == model)
        .map_or(0, |l| (l.usage.input_tokens + l.usage.cache_read_tokens + l.usage.cache_write_tokens + l.usage.output_tokens).max(0));
    Usage { cache_read_tokens: resent, output_tokens: 1, ..Usage::default() }
}

struct State {
    /// The session's own earlier turns.
    earlier: Spend,
    /// This turn's calls so far.
    turn: Spend,
    /// Its subagents', as last counted from their logs.
    subagents: Spend,
    /// Its subagents' calls since, as they reported them: without a limit
    /// the logs are not read again during the turn, and this is what the
    /// status bar adds.
    live: Spend,
    last: Option<LastCall>,
}

struct Inner {
    limits: BudgetLimits,
    session_id: String,
    sessions: PathBuf,
    pricer: Pricer,
    provider: String,
    model: String,
    state: Mutex<State>,
    /// How many of this turn's calls the host has recorded. The native loop
    /// waits for its own before it asks for the next: the host records a
    /// call when it handles the event, which can trail the loop.
    recorded: watch::Sender<u64>,
    /// The turn of the session that started this one, for a subagent: its
    /// limits are the parent's, counted over the parent's whole tree, so a
    /// subagent's call is judged where the parent's would be (R-SUB-4).
    parent: Option<Budget>,
    /// The session's tree, counted incrementally, for a budget with a limit.
    meter: Arc<Mutex<TreeMeter>>,
}

/// One turn's view of the session's spend, shared by the host, which
/// records every metered call, and the engine, which asks before each call
/// it makes. It prices whether or not a limit is set: the status bar's cost
/// is the same figure.
#[derive(Clone)]
pub struct Budget(Arc<Inner>);

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget").field("limits", &self.0.limits).field("session_id", &self.0.session_id).finish_non_exhaustive()
    }
}

/// What one recorded call leaves the session at.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The session and its subagents.
    pub total: Spend,
    pub turn: Spend,
}

impl Budget {
    /// The budget for a turn of `session_id` on `provider`/`model`, whose
    /// log so far is `events`.
    pub fn new(limits: BudgetLimits, session_id: &str, sessions: &Path, pricer: Pricer, provider: &str, model: &str, events: &[LogEvent]) -> Budget {
        let (earlier, last) = metered(events, &pricer);
        let mut meter = TreeMeter::default();
        // A session with nothing but its root has spawned nothing yet.
        let subagents = if events.len() > 1 { meter.spent(sessions, session_id, &pricer) } else { Spend::default() };
        Budget(Arc::new(Inner {
            limits,
            session_id: session_id.into(),
            sessions: sessions.into(),
            pricer,
            provider: provider.into(),
            model: model.into(),
            state: Mutex::new(State { earlier, turn: Spend::default(), subagents, live: Spend::default(), last }),
            recorded: watch::Sender::new(0),
            parent: None,
            meter: Arc::new(Mutex::new(meter)),
        }))
    }

    /// The budget of a subagent's turn: `parent`'s limits, held over the
    /// parent's tree — the parent's own spend, every subagent's, this one's
    /// included — rather than over this session alone. `events` is the
    /// subagent's log so far.
    pub fn for_subagent(parent: &Budget, session_id: &str, provider: &str, model: &str, events: &[LogEvent]) -> Budget {
        let p = &parent.0;
        let (earlier, last) = metered(events, &p.pricer);
        Budget(Arc::new(Inner {
            limits: p.limits,
            session_id: session_id.into(),
            sessions: p.sessions.clone(),
            pricer: p.pricer.clone(),
            provider: provider.into(),
            model: model.into(),
            state: Mutex::new(State { earlier, turn: Spend::default(), subagents: Spend::default(), live: Spend::default(), last }),
            recorded: watch::Sender::new(0),
            parent: Some(parent.clone()),
            meter: Arc::default(),
        }))
    }

    /// Where the session and its subagents stand now: what the status bar
    /// shows while subagents spend during the turn (R-SUB-4, R-BUDGET-2).
    /// With a limit, the subagents' logs counted again — from where the
    /// last count stopped — as the limit is judged; without one, the calls
    /// they reported as they made them, and no log is read.
    pub async fn refreshed(&self) -> Snapshot {
        if !self.0.limits.is_empty() {
            self.recount().await;
        }
        let s = self.state();
        Snapshot { total: Budget::total(&s), turn: s.turn.clone() }
    }

    /// The subagents' spend counted again from their logs, off the runtime,
    /// like any file work.
    async fn recount(&self) {
        let (sessions, id, pricer, meter) = (self.0.sessions.clone(), self.0.session_id.clone(), self.0.pricer.clone(), self.0.meter.clone());
        let subagents = tokio::task::spawn_blocking(move || meter.lock().unwrap_or_else(|e| e.into_inner()).spent(&sessions, &id, &pricer)).await.unwrap_or_default();
        let mut s = self.state();
        s.subagents = subagents;
    }

    /// A subagent's call, as it reports it: added to what its parents show
    /// while no limit has them read the logs again.
    fn record_descendant(&self, call: &Spend) {
        if !self.0.limits.is_empty() {
            return;
        }
        self.state().live.merge(call);
        if let Some(p) = &self.0.parent {
            p.record_descendant(call);
        }
    }

    pub fn limits(&self) -> BudgetLimits {
        self.0.limits
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn total(s: &State) -> Spend {
        let mut t = s.earlier.clone();
        t.merge(&s.subagents);
        t.merge(&s.live);
        t.merge(&s.turn);
        t
    }

    /// Records a call a backend's subagent made: spend like any other, but
    /// not the call the next one's floor is worked out from, and priced by
    /// the model that answered first — a subagent runs on its own.
    pub fn record_subagent(&self, answered: &str, usage: &Usage) -> Snapshot {
        let mut call = Spend::default();
        call.add(&self.0.pricer, &self.0.provider, answered, &self.0.model, usage);
        if let Some(p) = &self.0.parent {
            p.record_descendant(&call);
        }
        let mut s = self.state();
        s.turn.merge(&call);
        Snapshot { total: Budget::total(&s), turn: s.turn.clone() }
    }

    /// What the backend said the turn cost: counted when it is more than
    /// the turn's calls were priced at, or prices what they could not.
    pub fn reported(&self, usd: f64) -> Snapshot {
        let mut s = self.state();
        reconcile(&mut s.turn, usd);
        Snapshot { total: Budget::total(&s), turn: s.turn.clone() }
    }

    /// Records one metered call of this turn.
    pub fn record(&self, answered: &str, usage: &Usage) -> Snapshot {
        let mut call = Spend::default();
        call.add(&self.0.pricer, &self.0.provider, &self.0.model, answered, usage);
        if let Some(p) = &self.0.parent {
            p.record_descendant(&call);
        }
        let snap = {
            let mut s = self.state();
            s.turn.merge(&call);
            s.last = Some(LastCall { provider: self.0.provider.clone(), model: self.0.model.clone(), answered: answered.into(), usage: *usage });
            Snapshot { total: Budget::total(&s), turn: s.turn.clone() }
        };
        self.0.recorded.send_modify(|n| *n += 1);
        snap
    }

    /// Before a model call: refused when what the session and its
    /// subagents have spent, plus the least the call costs, would go past a
    /// limit. `made` is how many calls this turn has made so far; their
    /// usage is waited for first, so none is missed.
    pub async fn admit(&self, made: u64) -> Result<(), EngineError> {
        self.check_before(made, false).await
    }

    /// Before a backend's turn. Its model is often an alias the price list
    /// does not name (`sonnet`), and its calls are the vendor's: with no
    /// call metered yet the floor is zero — `over` interrupts it once its
    /// first priced call goes past — and after one, the floor is priced by
    /// the id that answered.
    pub async fn admit_turn(&self) -> Result<(), EngineError> {
        self.check_before(0, true).await
    }

    async fn check_before(&self, made: u64, backend: bool) -> Result<(), EngineError> {
        if self.0.limits.is_empty() {
            return Ok(());
        }
        let mut rx = self.0.recorded.subscribe();
        let _ = rx.wait_for(|n| *n >= made).await;
        let (next, answered) = {
            let s = self.state();
            let answered = s.last.as_ref().filter(|l| l.provider == self.0.provider && l.model == self.0.model).map(|l| l.answered.clone());
            (floor(s.last.as_ref(), &self.0.provider, &self.0.model), answered)
        };
        if backend && answered.is_none() {
            return self.judge_tree(None).await;
        }
        let price = (self.0.pricer)(&self.0.provider, &self.0.model, &next).or_else(|| answered.and_then(|a| (self.0.pricer)(&self.0.provider, &a, &next)));
        let model = format!("{}/{}", self.0.provider, self.0.model);
        self.judge_tree(Some((next, price, model))).await
    }

    /// Judges the call about to be made against the limits, over the tree
    /// the limits hold: a subagent asks its parent, which asks its own, up
    /// to the session the limits were set on. There the subagents are
    /// counted again — one spends during its parent's turn — off the
    /// runtime, like any file work; a subagent's own calls are in its log
    /// before they are recorded, so the count has them.
    fn judge_tree(&self, next: Option<(Usage, Option<f64>, String)>) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            if let Some(parent) = &self.0.parent {
                return parent.judge_tree(next).await;
            }
            self.recount().await;
            let spent = Budget::total(&self.state());
            match judge(&self.0.limits, &spent, next.as_ref().map(|(u, p, m)| (u, *p, m.as_str()))) {
                None => Ok(()),
                Some(trip) => Err(self.refusal(&trip, true)),
            }
        })
    }

    /// After a metered call: whether the session is already past a limit —
    /// for a backend, whose next call is not krowk's to refuse, the moment
    /// to interrupt it.
    pub fn over(&self) -> Option<EngineError> {
        let spent = Budget::total(&self.state());
        judge(&self.0.limits, &spent, None).map(|t| self.refusal(&t, false))
    }

    /// What a turn stopped by the budget ends with: the code and exit status
    /// of `krowk sessions budget`, its sentence, and the next action.
    fn refusal(&self, trip: &Trip, before_call: bool) -> EngineError {
        let id = &self.0.session_id;
        let stopped = if before_call { "krowk stopped it before that call; what it produced is kept" } else { "krowk interrupted the turn; what it produced is kept" };
        let fix = match trip {
            Trip::Unknown { .. } => format!(
                "refresh the prices with `krowk pricing refresh`, or hold it to --max-tokens instead: `krowk -p --resume {id} --max-tokens <count> \"…\"`"
            ),
            _ => format!("to go on, raise the limit: `krowk -p --resume {id} {} <more> \"…\"`", trip.flag()),
        };
        EngineError::new(EXCEEDED, format!("session {id} is over budget: {} — {stopped}; {fix}", trip.sentence()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ModelRef;

    fn spend(output: i64, reasoning: i64, usd: Option<f64>) -> Spend {
        let mut s = Spend { usage: Usage { output_tokens: output, reasoning_tokens: reasoning, ..Usage::default() }, ..Spend::default() };
        match usd {
            Some(u) => s.known_usd = u,
            None => {
                s.unpriced.insert("p/m".into());
            }
        }
        s
    }

    #[test]
    fn r_budget_1_a_call_is_refused_when_its_floor_would_cross_the_limit_and_at_the_limit_is_inside() {
        let one = Usage { output_tokens: 1, ..Usage::default() };
        let tokens = BudgetLimits { max_tokens: Some(100), max_usd: None };
        // 99 generated: the next call's one token lands on the limit — allowed.
        assert_eq!(judge(&tokens, &spend(90, 9, Some(0.0)), Some((&one, Some(0.0), "p/m"))), None);
        // 100 generated: at the limit is inside it, but the next call would cross.
        assert_eq!(judge(&tokens, &spend(100, 0, Some(0.0)), None), None, "at the limit, after a call, is inside it");
        assert_eq!(
            judge(&tokens, &spend(60, 40, Some(0.0)), Some((&one, Some(0.0), "p/m"))),
            Some(Trip::Tokens { generated: 100, limit: 100, next: true }),
            "reasoning counts: it is the part that overshoots"
        );
        let usd_limit = BudgetLimits { max_usd: Some(0.01), max_tokens: None };
        assert_eq!(judge(&usd_limit, &spend(0, 0, Some(0.009)), Some((&one, Some(0.0009), "p/m"))), None);
        assert_eq!(
            judge(&usd_limit, &spend(0, 0, Some(0.009)), Some((&one, Some(0.0011), "p/m"))),
            Some(Trip::Usd { spent: 0.009, next: Some(0.0011), limit: 0.01 }),
            "compared unrounded: $0.0101 is over $0.01"
        );
        // An unknown price trips a dollar limit — spent or about to be.
        assert!(matches!(judge(&usd_limit, &spend(0, 0, None), None), Some(Trip::Unknown { .. })));
        assert_eq!(
            judge(&usd_limit, &spend(0, 0, Some(0.0)), Some((&one, None, "x/unpriced"))),
            Some(Trip::Unknown { known: 0.0, unpriced: vec!["x/unpriced".into()] })
        );
        assert_eq!(judge(&BudgetLimits::default(), &spend(1 << 40, 0, None), Some((&one, None, "p/m"))), None, "no limit, no check");
    }

    #[test]
    fn r_budget_1_the_floor_is_the_last_prompt_resent_from_cache_and_one_token() {
        let last = LastCall { provider: "anthropic".into(), model: "m".into(), answered: "m-2026".into(), usage: Usage { input_tokens: 10, cache_read_tokens: 3000, cache_write_tokens: 40, output_tokens: 50, reasoning_tokens: 900 } };
        assert_eq!(floor(Some(&last), "anthropic", "m"), Usage { cache_read_tokens: 3100, output_tokens: 1, ..Usage::default() });
        assert_eq!(floor(Some(&last), "anthropic", "other"), Usage { output_tokens: 1, ..Usage::default() }, "another model counts tokens its own way");
        assert_eq!(floor(None, "anthropic", "m"), Usage { output_tokens: 1, ..Usage::default() });
    }

    #[test]
    fn r_budget_1_a_backend_alias_is_priced_by_the_model_that_answered_and_its_reported_total_counts() {
        let dir = std::env::temp_dir().join(format!("krowk-budget-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (_, root) = log::SessionLog::create(&dir, Path::new("/repo"), "t").unwrap();
        // Prices the snapshot id only, as models.dev does: `sonnet` is Claude
        // Code's alias for it.
        let pricer: Pricer = Arc::new(|_, m: &str, u: &Usage| (m == "claude-sonnet-4-6").then(|| u.output_tokens as f64 / 1000.0 + u.cache_read_tokens as f64 / 1e6));
        let limits = BudgetLimits { max_usd: Some(0.10), max_tokens: None };
        let b = Budget::new(limits, &root.session_id, &dir, pricer.clone(), "anthropic", "sonnet", std::slice::from_ref(&root));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert!(rt.block_on(b.admit_turn()).is_ok(), "no call yet: the floor is zero, not an unknown price");
        assert!(matches!(rt.block_on(b.admit(0)), Err(e) if e.message.contains("no price for anthropic/sonnet")), "a native call on an unpriced id still trips");
        let snap = b.record("claude-sonnet-4-6", &Usage { output_tokens: 50, cache_read_tokens: 1000, ..Usage::default() });
        assert!((snap.turn.cost().unwrap() - 0.051).abs() < 1e-9, "priced by the answering id: {:?}", snap.turn);
        assert!(rt.block_on(b.admit_turn()).is_ok(), "the next floor is priced by the answering id too");
        // A subagent's call counts, and the vendor's reported total wins when larger.
        b.record_subagent("claude-sonnet-4-6", &Usage { output_tokens: 20, ..Usage::default() });
        let snap = b.reported(0.12);
        assert_eq!(snap.turn.cost(), Some(0.12));
        assert!(b.over().is_some_and(|e| e.message.contains("over --max-usd $0.100")), "past the limit on the reported total");
        assert_eq!(b.reported(0.01).turn.cost(), Some(0.12), "a smaller reported total changes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r_budget_1_a_subagent_is_priced_by_the_model_it_ran_on() {
        let dir = std::env::temp_dir().join(format!("krowk-budget-subagent-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Sonnet a dollar per thousand output tokens, Haiku a tenth of that.
        let pricer: Pricer = Arc::new(|_, m: &str, u: &Usage| match m {
            "claude-sonnet-4-6" => Some(u.output_tokens as f64 / 1000.0),
            "claude-haiku-4-5" => Some(u.output_tokens as f64 / 10_000.0),
            _ => None,
        });
        let (mut log, root) = log::SessionLog::create(&dir, Path::new("/repo"), "t").unwrap();
        let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
        log.append(LogBody::TurnStarted { turn_id: "t1".into(), model, provider: "anthropic".into(), wire_api: crate::protocol::WireApi::ClaudeCode, permission_mode: Default::default(), effort: None, rollover: None }).unwrap();
        log.append(LogBody::SubagentResponse { turn_id: "t1".into(), response_id: None, model: "claude-haiku-4-5".into(), usage: Usage { output_tokens: 1000, ..Usage::default() } }).unwrap();
        drop(log);
        let events = log::read_events(&dir.join(&root.session_id).join(log::EVENTS_FILE)).unwrap();
        let (spent, _) = metered(&events, &pricer);
        assert!((spent.cost().unwrap() - 0.1).abs() < 1e-9, "from the log, at Haiku's price: {spent:?}");
        let b = Budget::new(BudgetLimits::default(), &root.session_id, &dir, pricer, "anthropic", "claude-sonnet-4-6", &[]);
        let snap = b.record_subagent("claude-haiku-4-5", &Usage { output_tokens: 1000, ..Usage::default() });
        assert!((snap.turn.cost().unwrap() - 0.1).abs() < 1e-9, "live, at Haiku's price: {:?}", snap.turn);
        let snap = b.record_subagent("claude-opus-9", &Usage { output_tokens: 1000, ..Usage::default() });
        assert!((snap.turn.cost().unwrap() - 1.1).abs() < 1e-9, "an unpriced answer falls back to the session's model");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pricer() -> Pricer {
        // A dollar per thousand generated tokens; nothing else costs.
        Arc::new(|_, model: &str, u: &Usage| (model != "free-lunch").then(|| (u.output_tokens + u.reasoning_tokens) as f64 / 1000.0))
    }

    fn write_session(sessions: &Path, parent: Option<&str>, output: i64) -> String {
        let (mut log, root) = log::SessionLog::create_child(sessions, Path::new("/repo"), "t", parent, None).unwrap();
        let turn = "t1".to_string();
        let model = ModelRef { instance: "anthropic".into(), model: "m".into() };
        log.append(LogBody::TurnStarted { turn_id: turn.clone(), model, provider: "anthropic".into(), wire_api: crate::protocol::WireApi::AnthropicMessages, permission_mode: Default::default(), effort: None, rollover: None }).unwrap();
        log.append(LogBody::ResponseCompleted { turn_id: turn, response_id: None, model: "m".into(), usage: Usage { output_tokens: output, ..Usage::default() }, stop_reason: None, item_ids: vec![] }).unwrap();
        root.session_id
    }

    #[test]
    fn r_sub_4_the_tree_is_counted_as_its_logs_grow_and_without_a_limit_not_read_at_all() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("krowk-budget-meter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let parent = write_session(&dir, None, 100);
        let child = write_session(&dir, Some(&parent), 200);
        let mut meter = TreeMeter::default();
        assert_eq!(meter.spent(&dir, &parent, &pricer()).generated(), 200);
        let at = meter.logs[&child].offset;
        assert_eq!(at, std::fs::metadata(dir.join(&child).join(log::EVENTS_FILE)).unwrap().len(), "read to the end once");
        // A torn line waits for the next count; a whole one is read from
        // where the last count stopped.
        let line = serde_json::to_string(&LogEvent {
            id: krowk_store::new_id(),
            parent_id: None,
            session_id: child.clone(),
            time_ms: 0,
            body: LogBody::ResponseCompleted { turn_id: "t1".into(), response_id: None, model: "m".into(), usage: Usage { output_tokens: 50, ..Usage::default() }, stop_reason: None, item_ids: vec![] },
        })
        .unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join(&child).join(log::EVENTS_FILE)).unwrap();
        f.write_all(&line.as_bytes()[..20]).unwrap();
        assert_eq!(meter.spent(&dir, &parent, &pricer()).generated(), 200);
        assert_eq!(meter.logs[&child].offset, at);
        f.write_all(&line.as_bytes()[20..]).unwrap();
        f.write_all(b"\n").unwrap();
        assert_eq!(meter.spent(&dir, &parent, &pricer()).generated(), 250);
        // Without a limit a subagent's calls reach its parent as reported,
        // and the logs are not read again.
        let (_, events) = log::SessionLog::open(&dir, &parent).unwrap();
        let root = Budget::new(BudgetLimits::default(), &parent, &dir, pricer(), "anthropic", "m", &events);
        let kid = Budget::for_subagent(&root, &child, "anthropic", "m", &[]);
        kid.record("m", &Usage { output_tokens: 7, ..Usage::default() });
        std::fs::remove_dir_all(dir.join(&child)).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let snap = rt.block_on(root.refreshed());
        assert_eq!(snap.total.generated(), 100 + 250 + 7, "the count at the turn's start, and the call as reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r_budget_1_a_subagents_spend_counts_toward_its_parents_budget() {
        let dir = std::env::temp_dir().join(format!("krowk-budget-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let parent = write_session(&dir, None, 100);
        let child = write_session(&dir, Some(&parent), 200);
        let grandchild = write_session(&dir, Some(&child), 300);
        let _stranger = write_session(&dir, None, 5000);
        let found: Vec<String> = descendants(&dir, &parent).into_iter().map(|(id, _)| id).collect();
        assert_eq!(found, [child.clone(), grandchild], "subagents, and theirs; nobody else's");
        let (_, events) = log::SessionLog::open(&dir, &parent).unwrap();
        let limits = BudgetLimits { max_tokens: Some(650), max_usd: None };
        let b = Budget::new(limits, &parent, &dir, pricer(), "anthropic", "m", &events);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        // 100 + 200 + 300 = 600 generated: a call fits.
        assert!(rt.block_on(b.admit(0)).is_ok());
        // The child spends 50 more while the parent's turn runs: 650, and the
        // next call would cross.
        let (mut clog, _) = log::SessionLog::open(&dir, &child).unwrap();
        clog.append(LogBody::ResponseCompleted { turn_id: "t1".into(), response_id: None, model: "m".into(), usage: Usage { output_tokens: 50, ..Usage::default() }, stop_reason: None, item_ids: vec![] }).unwrap();
        let e = rt.block_on(b.admit(0)).unwrap_err();
        assert_eq!(e.code, EXCEEDED);
        assert!(e.message.contains("650 tokens generated of --max-tokens 650") && e.message.contains(&format!("krowk -p --resume {parent} --max-tokens")), "{}", e.message);
        // Priced the same way: $0.65 across the tree.
        let snap = b.record("m", &Usage::default());
        assert!((snap.total.cost().unwrap() - 0.65).abs() < 1e-9, "{:?}", snap.total);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
