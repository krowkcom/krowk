//! Subagents (R-SUB-1 … R-SUB-5): the `subagent` tool, which starts a
//! child session to do one task and hands only its final summary back.
//!
//! A subagent is a session like any other — its own log, its own context,
//! its own model — whose root names the session that started it
//! (`session.started.parentSessionId`), and whose start the parent logs as
//! `subagent.started`, naming the child and the tool call it answers. The
//! link both ways is what keeps a session tree together: the budget counts
//! the tree (R-SUB-4), `krowk sessions rebuild` restores it from the logs
//! alone, and moving or syncing a session takes its children with it
//! (R-SUB-6). Everything else about a subagent is its own log's.
//!
//! - **Its own context.** The child is handed the prompt the parent's model
//!   wrote and nothing else — not the parent's conversation — and the
//!   parent's context grows by the call and the summary only: the child's
//!   reads and searches stay in the child's log.
//! - **Its own model**: the agent definition's, else the config's
//!   `subagents.model`, else the catalog's cheaper tier below the parent's
//!   model, else the parent's (`choose_model`). Always krowk's own loop: a
//!   vendor backend runs its own agents, and could not be held to the
//!   allowlist.
//! - **Its own tools**: the definition's allowlist, else every tool but
//!   this one — subagents start no subagents of their own. The permission
//!   mode is the parent's, never a looser one, and the file tools' fences
//!   are the same inside a subagent: an allowlist narrows, it never grants.
//! - **In parallel**: the subagent calls of one response run at once, up to
//!   `subagents.maxParallel` (4 by default) at a time (R-SUB-2).
//! - **Interruptible one by one**: each child's turn is a running turn of
//!   the host, so `interrupt` with the child's session id stops that child
//!   alone, and its call is answered with what it had; interrupting the
//!   parent interrupts every child it is waiting on.
//! - **Visible**: every line of a child's stream goes to the parent's
//!   client under the child's session id, which is how the TUI draws each
//!   subagent's line (R-SUB-3).
//! - **In a worktree of its own, when asked** (`isolation: "worktree"`, on
//!   the call or in the definition, the call's winning): the child works in
//!   a new git worktree of the parent's repository on a `krowk/<hex>`
//!   branch (`crate::worktree`), so parallel children that edit do not
//!   overwrite each other. It starts from the parent's files, uncommitted
//!   changes included, so a child started mid-edit sees the edit. One it left unchanged is removed when it ends;
//!   a changed one is kept, and its summary says where.

use crate::agents::AgentDef;
use crate::budget::Budget;
use crate::catalog::{self, Listed};
use crate::engine::Events;
use crate::evidence::Evidence;
use crate::host::Shared;
use crate::instances::{Registry, ALIASES};
use crate::protocol::{ModelRef, PermissionMode, StreamLine, TurnStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Semaphore};

pub const SUBAGENT: &str = "subagent";

/// What the model is told. Short: it rides on every call. The agent
/// definitions there are, when any, are listed after it.
pub const DESCRIPTION: &str = "Start a subagent with a fresh context for one task; only its final summary comes back. Use it for searches and reviews that would fill your context. Calls in one response run in parallel.";

/// Subagents at once, per parent turn, when the config does not say.
pub const MAX_PARALLEL: usize = 4;

/// Start a subagent.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentInput {
    /// 3-5 words, shown to the person.
    pub description: String,
    /// The whole task: it sees nothing else.
    pub prompt: String,
    /// An agent definition's name.
    #[serde(default)]
    pub agent: Option<String>,
    /// worktree: its own git worktree and branch.
    #[serde(default)]
    #[schemars(with = "Isolation")]
    pub isolation: Option<Isolation>,
}

/// Where a subagent works: the parent's directory, or a git worktree of
/// its own (`crate::worktree`). Claude Code's agent files spell it the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    #[default]
    None,
    Worktree,
}

/// What a subagent is told when it is asked for a worktree outside one.
pub const NEEDS_A_REPOSITORY: &str = "isolation: worktree needs a git repository";

/// What a subagent's turn runs with, from its definition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRun {
    /// The definition's name, or none for a general subagent.
    pub name: Option<String>,
    /// The definition's body, after krowk's own prompt.
    pub instructions: String,
    /// The tools it may use, krowk's names with the edit tool as `edit`;
    /// none is every tool but `subagent`.
    pub tools: Option<Vec<String>>,
    /// The session that started it, and its log: what a hook is told the
    /// session is, as Claude Code tells a subagent's hooks — the
    /// subagent's own id rides beside it as `agent_session_id`.
    pub parent_session: String,
    pub parent_transcript: String,
}

impl AgentRun {
    /// Whether the turn offers the tool `name`, whose edit tool is `edit`.
    pub fn allows(&self, name: &str, edit: &str) -> bool {
        if name == SUBAGENT {
            return false;
        }
        match &self.tools {
            None => true,
            Some(t) => t.iter().any(|a| a == name || (a == "edit" && name == edit)),
        }
    }
}

/// What a subagent is told after krowk's own system prompt: that only its
/// last message goes back, then its definition's instructions.
pub fn system_prompt(base: &str, run: &AgentRun) -> String {
    let mut s = format!("{base}\nYou are a subagent: only your final message goes back to the agent that started you, so end with a complete, self-contained summary of what you found or did.");
    if !run.instructions.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(run.instructions.trim());
    }
    s
}

/// Every model the catalog lists for a provider, for choosing a
/// subagent's model: supplied by the caller, which owns the models.dev cache.
pub type Models = Arc<dyn Fn(&str) -> Vec<Listed> + Send + Sync>;

/// Where agent definitions come from beyond the repository, and how a
/// subagent's model is chosen.
#[derive(Clone)]
pub struct AgentsConfig {
    /// The person's own definition directories — krowk's, then Claude
    /// Code's — read after the repository's.
    pub user_dirs: Vec<PathBuf>,
    pub models: Models,
    /// Where a subagent's worktree is made (`krowk_api::home::worktrees_root`);
    /// none refuses `isolation: "worktree"`.
    pub worktrees: Option<PathBuf>,
}

impl AgentsConfig {
    /// No definitions of the person's, and a catalog that lists nothing:
    /// subagents run on the parent's model unless told otherwise, and in
    /// no worktree.
    pub fn none() -> AgentsConfig {
        AgentsConfig { user_dirs: Vec::new(), models: Arc::new(|_| Vec::new()), worktrees: None }
    }
}

/// The model a subagent runs on. `asked` is the definition's `model`,
/// `config` the config's `subagents.model`; with neither, the cheaper tier
/// the catalog lists below the parent's model, else the parent's model.
/// `inherit` is the parent's; a Claude Code alias is the newest model of
/// its family on the parent's instance, else on `anthropic`.
pub fn choose_model(asked: Option<&str>, config: Option<&str>, parent: &ModelRef, parent_provider: &str, registry: &Registry, listed: &dyn Fn(&str) -> Vec<Listed>) -> Result<ModelRef, String> {
    let named = asked.or(config).map(str::trim).filter(|m| !m.is_empty());
    match named {
        None => Ok(catalog::cheaper(&listed(parent_provider), &parent.model).map_or_else(|| parent.clone(), |m| ModelRef { instance: parent.instance.clone(), model: m })),
        Some("inherit") => Ok(parent.clone()),
        Some(a) if ALIASES.contains(&a.to_ascii_lowercase().as_str()) => {
            let family = format!("claude-{}", a.to_ascii_lowercase());
            if let Some(m) = catalog::newest(&listed(parent_provider), &family) {
                return Ok(ModelRef { instance: parent.instance.clone(), model: m });
            }
            let anthropic = registry.get("anthropic").map_err(|e| format!("model {a:?}: {e}"))?;
            match catalog::newest(&listed(&anthropic.provider), &family) {
                Some(m) => Ok(ModelRef { instance: "anthropic".into(), model: m }),
                None => Err(format!("model {a:?} names the {family} family, which the model catalog does not list — run `krowk pricing refresh`, or name the model as <instance>/<model>")),
            }
        }
        Some(m) => registry.parse_model(m),
    }
}

/// Agent definitions listed in the tool's description, at most: every one
/// rides on every call.
pub const MAX_LISTED: usize = 20;
/// Characters of a definition's description listed, at most: its first
/// paragraph, cut here.
pub const MAX_DESCRIPTION: usize = 200;
/// Definitions named in it at all, the described ones included.
pub const MAX_NAMED: usize = 200;

/// The `subagent` tool's description with the definitions listed after
/// it, each its first paragraph cut to `MAX_DESCRIPTION` characters, the
/// first `MAX_LISTED` of them: a repository with many definitions, or one
/// long one, costs every call a bounded amount.
pub fn describe(defs: &[AgentDef]) -> String {
    if defs.is_empty() {
        return DESCRIPTION.into();
    }
    let mut s = format!("{DESCRIPTION}\nAgents (name them in `agent`):");
    for d in defs.iter().take(MAX_LISTED) {
        let first = d.description.split("\n\n").next().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
        let cut: String = first.chars().take(MAX_DESCRIPTION).collect();
        let more = if cut.len() < first.len() { "…" } else { "" };
        s.push_str(&format!("\n- {}: {cut}{more}", d.name));
    }
    // The rest by name alone, so each can still be called — up to a bound
    // of its own: a name is short, a thousand of them are not.
    if defs.len() > MAX_LISTED {
        let rest: Vec<&str> = defs[MAX_LISTED..].iter().take(MAX_NAMED - MAX_LISTED).map(|d| d.name.as_str()).collect();
        s.push_str(&format!("\nAlso: {}", rest.join(", ")));
        if defs.len() > MAX_NAMED {
            s.push_str(&format!(" (and {} more)", defs.len() - MAX_NAMED));
        }
    }
    s
}

/// What a turn may start subagents with: handed to the engine in
/// `TurnContext::subagents`, and cheap to clone.
#[derive(Clone)]
pub struct Subagents(pub(crate) Arc<Spawn>);

impl std::fmt::Debug for Subagents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subagents").field("parent", &self.0.parent.session_id).field("agents", &self.0.defs.len()).finish_non_exhaustive()
    }
}

/// The parent turn a subagent is started from.
pub(crate) struct ParentTurn {
    pub session_id: String,
    pub turn_id: String,
    pub model: ModelRef,
    /// The provider the parent's instance is priced and described under.
    pub provider: String,
    pub cwd: PathBuf,
    pub permission_mode: PermissionMode,
    pub budget: Budget,
    pub evidence: Option<Evidence>,
    /// The parent's permissions, its instructions, skills and hooks, and
    /// its session's grants: what a subagent's calls are judged by.
    pub gate: crate::permissions::Gate,
    pub compat: Arc<crate::compat::Compat>,
    pub grants: crate::permissions::SessionGrants,
    /// Flips when the parent's turn is interrupted: every child follows.
    pub cancel: watch::Receiver<bool>,
}

pub(crate) struct Spawn {
    pub host: Arc<Shared>,
    pub parent: ParentTurn,
    /// The parent's client: every line of every child goes there too.
    pub out: mpsc::Sender<StreamLine>,
    /// Holds the fan-out to `subagents.maxParallel` at once.
    pub gate: Arc<Semaphore>,
    /// The agent definitions the turn found, first by name winning.
    pub defs: Vec<AgentDef>,
    /// What this turn's subagents cost between them, and whether any of it
    /// had no price: part of the parent turn's cost, as a backend's own
    /// subagents are part of theirs.
    pub spent: std::sync::Mutex<(f64, bool)>,
}

impl Subagents {
    /// What the turn's subagents have cost so far: USD, and whether any of
    /// it had no price.
    pub fn spent(&self) -> (f64, bool) {
        *self.0.spent.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The definition a call's `agent` names, found the one way the
    /// permission rules judge it too: trimmed, and regardless of case, so
    /// ` Reviewer ` is the `reviewer` a `Task(reviewer)` rule names. None
    /// for a general subagent; a name no definition has is refused.
    pub fn resolve(&self, agent: Option<&str>) -> Result<Option<&AgentDef>, String> {
        let Some(name) = agent.map(str::trim).filter(|a| !a.is_empty()) else { return Ok(None) };
        match self.0.defs.iter().find(|d| d.name == name).or_else(|| self.0.defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))) {
            Some(d) => Ok(Some(d)),
            None => {
                let names: Vec<&str> = self.0.defs.iter().map(|d| d.name.as_str()).collect();
                let there = if names.is_empty() { "there are no agent definitions".to_string() } else { format!("the agents are {}", names.join(", ")) };
                Err(format!("there is no agent named {name:?} — {there}; leave `agent` out for a general subagent"))
            }
        }
    }

    /// The tool's description, listing the agent definitions there are.
    pub fn description(&self) -> String {
        describe(&self.0.defs)
    }

    /// One `subagent` call: the child's final summary, or why there is none.
    pub async fn run(&self, call_id: &str, input: &Value, events: &Events) -> (String, bool) {
        let input = match SubagentInput::deserialize(input) {
            Ok(i) => i,
            Err(e) => return (format!("invalid input for subagent: {e}"), true),
        };
        if input.prompt.trim().is_empty() {
            return ("subagent needs a prompt: the whole task, since the subagent sees nothing else".into(), true);
        }
        let def = match self.resolve(input.agent.as_deref()) {
            Ok(d) => d,
            Err(why) => return (why, true),
        };
        let host = &self.0.host;
        let cfg = &host.cfg;
        let p = &self.0.parent;
        let registry = host.registry();
        let choose = |asked| choose_model(asked, registry.subagents.model.as_deref(), &p.model, &p.provider, &registry, cfg.agents.models.as_ref());
        let model = match choose(def.and_then(|d| d.model.as_deref())) {
            Ok(m) => m,
            Err(e) => return (format!("the subagent could not start: {e}"), true),
        };
        // A repository's definition is someone else's words until the
        // repository is trusted: it may pick a model of the parent's own
        // instance — the account the person chose — and no other, so a
        // cloned repository cannot send its prompt to another provider on
        // another key. Anywhere else it runs on the default instead.
        let model = match def {
            // Trusted as the permission rules judge it: the repository's own
            // allow rules, directories and hooks count only then too.
            Some(d) if d.project && model.instance != p.model.instance && !cfg.permissions.trusted.as_ref().is_some_and(|t| t(&crate::trust::root(&p.cwd))) => {
                let fallback = match choose(None) {
                    Ok(m) => m,
                    Err(e) => return (format!("the subagent could not start: {e}"), true),
                };
                let _ = events
                    .send(crate::engine::EngineEvent::Notice {
                        text: format!(
                            "agent {} asks for {model}, on another instance than this session's, and {} is not a repository you have trusted — it runs on {fallback} instead",
                            d.name,
                            crate::trust::root(&p.cwd).display()
                        ),
                    })
                    .await;
                fallback
            }
            _ => model,
        };
        let run = AgentRun {
            name: def.map(|d| d.name.clone()),
            instructions: def.map(|d| d.instructions.clone()).unwrap_or_default(),
            tools: def.and_then(|d| d.tools.clone()),
            parent_session: p.session_id.clone(),
            parent_transcript: p.compat.transcript.clone(),
        };
        // Waits its turn behind the fan-out limit, unless the parent is
        // interrupted first.
        let mut cancel = p.cancel.clone();
        let _permit = tokio::select! {
            permit = self.0.gate.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return ("not run: the turn is ending".into(), true),
            },
            _ = crate::engine::cancelled(&mut cancel) => return ("not run: the turn was interrupted".into(), true),
        };
        // Its session id is chosen now: a worktree is locked in its name
        // before the session exists, and the session records it as its
        // directory.
        let child = krowk_store::new_id();
        let (worktree, prompt) = match input.isolation.or(def.and_then(|d| d.isolation)).unwrap_or_default() {
            Isolation::None => (None, input.prompt.clone()),
            Isolation::Worktree => match self.worktree(&child).await {
                Ok((w, notes)) => (Some(w), crate::worktree::first_prompt(&notes, &input.prompt)),
                Err(why) => return (why, true),
            },
        };
        let cwd = worktree.as_ref().map_or_else(|| p.cwd.clone(), |w| w.path.clone());
        let result = host.subagent(&self.0, call_id, &input.description, &prompt, model, run, (&child, &cwd), events).await;
        if let Ok(r) = &result {
            let mut spent = self.0.spent.lock().unwrap_or_else(|e| e.into_inner());
            match r.cost_usd {
                Some(usd) => spent.0 += usd,
                None => spent.1 = true,
            }
        }
        let (text, failed) = answer(result);
        match worktree {
            Some(w) => match finish(w).await {
                Some(note) => (format!("{text}\n\n{note}"), failed),
                None => (text, failed),
            },
            None => (text, failed),
        }
    }

    /// A worktree of the parent's repository for the child `child`, readied
    /// by the prepare steps, with their notes for its first prompt; why
    /// not, for the model, when there is none.
    async fn worktree(&self, child: &str) -> Result<(crate::worktree::Worktree, Vec<String>), String> {
        let Some(root) = self.0.host.cfg.agents.worktrees.clone() else {
            return Err("isolation: worktree has nowhere to make worktrees: krowk found no home directory (set HOME or XDG_DATA_HOME)".into());
        };
        let (cwd, child, config) = (self.0.parent.cwd.clone(), child.to_string(), self.0.host.registry().worktrees.clone());
        let made = tokio::task::spawn_blocking(move || {
            let w = crate::worktree::create(&cwd, &root, &child)?;
            let notes = crate::worktree::prepare(&crate::worktree::Prepare { worktree: &w, config: &config });
            Ok((w, notes))
        })
        .await;
        match made {
            Ok(Ok(made)) => Ok(made),
            Ok(Err(crate::worktree::Error::NotARepository)) => Err(NEEDS_A_REPOSITORY.into()),
            Ok(Err(e)) => Err(format!("the subagent's worktree could not be made: {e}")),
            Err(e) => Err(format!("the subagent's worktree could not be made: {e}")),
        }
    }
}

/// What a child's turn answers its call with: its summary, or why there
/// is none.
fn answer(result: Result<crate::protocol::RunResult, crate::engine::EngineError>) -> (String, bool) {
    match result {
        Ok(r) => match r.status {
            TurnStatus::Completed if r.result.trim().is_empty() => ("the subagent finished without a summary".into(), false),
            TurnStatus::Completed => (r.result, false),
            TurnStatus::Interrupted if r.result.trim().is_empty() => ("the subagent was interrupted before it said anything".into(), true),
            TurnStatus::Interrupted => (format!("the subagent was interrupted; what it said last:\n{}", r.result), true),
            TurnStatus::Failed => (format!("the subagent failed: {}", r.error.map(|e| e.message).unwrap_or_default()), true),
        },
        Err(e) => (format!("the subagent could not start: {}", e.message), true),
    }
}

/// A child's worktree, finished when the child is (`crate::worktree::finish`):
/// what its parent is told of it, when it is kept.
async fn finish(w: crate::worktree::Worktree) -> Option<String> {
    let path = w.path.clone();
    match tokio::task::spawn_blocking(move || crate::worktree::finish(&w).map(|f| f.note(&w))).await {
        Ok(Ok(note)) => note,
        Ok(Err(e)) => Some(format!("Worktree: {} was left as it is: {e}", path.display())),
        Err(e) => Some(format!("Worktree: {} was left as it is: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::InstancesConfig;

    #[test]
    fn r_sub_1_the_model_is_the_definitions_else_the_configs_else_a_cheaper_tier() {
        let registry = Registry::resolve(&InstancesConfig::default(), &|_| String::new());
        let listed = |p: &str| -> Vec<Listed> {
            let m = |id: &str, family: &str, released: &str, price: f64| Listed { id: id.into(), family: family.into(), released: released.into(), output_price: Some(price), agentic: true };
            match p {
                "anthropic" => vec![m("claude-opus-5-5", "claude-opus", "2026-09-22", 20.0), m("claude-sonnet-5", "claude-sonnet", "2026-06-29", 10.0), m("claude-haiku-4-5", "claude-haiku", "2025-10-15", 5.0)],
                _ => Vec::new(),
            }
        };
        let parent = ModelRef { instance: "anthropic".into(), model: "claude-opus-5-5".into() };
        let pick = |asked, config| choose_model(asked, config, &parent, "anthropic", &registry, &listed);
        assert_eq!(pick(None, None).unwrap().model, "claude-sonnet-5", "the cheaper tier by default");
        assert_eq!(pick(Some("inherit"), Some("anthropic/claude-haiku-4-5")).unwrap(), parent, "the definition outranks the config");
        assert_eq!(pick(None, Some("anthropic/claude-haiku-4-5")).unwrap().model, "claude-haiku-4-5");
        assert_eq!(pick(Some("haiku"), None).unwrap(), ModelRef { instance: "anthropic".into(), model: "claude-haiku-4-5".into() }, "Claude Code's alias, the newest of its family");
        assert_eq!(pick(Some("gpt-5.5"), None).unwrap(), ModelRef { instance: "openai".into(), model: "gpt-5.5".into() });
        assert!(pick(Some("fable"), None).unwrap_err().contains("claude-fable"));
        // A parent the catalog does not know runs its subagents on itself.
        let router = ModelRef { instance: "openrouter".into(), model: "house/coder".into() };
        assert_eq!(choose_model(None, None, &router, "openrouter", &registry, &listed).unwrap(), router);
        // An alias on a GPT parent goes to the anthropic instance.
        let gpt = ModelRef { instance: "openai".into(), model: "gpt-5.5".into() };
        assert_eq!(choose_model(Some("sonnet"), None, &gpt, "openai", &registry, &listed).unwrap(), ModelRef { instance: "anthropic".into(), model: "claude-sonnet-5".into() });
    }

    #[test]
    fn r_sub_5_the_definitions_listed_cost_every_call_a_bounded_amount() {
        let def = |i: usize, description: String| AgentDef { name: format!("agent-{i:02}"), description, model: None, tools: None, isolation: None, instructions: String::new(), path: PathBuf::new(), project: true };
        let long = format!("{}\n\nA second paragraph nobody lists.", "word ".repeat(200));
        let many: Vec<AgentDef> = (0..50).map(|i| def(i, long.clone())).collect();
        let text = describe(&many);
        let per = MAX_DESCRIPTION + "\n- agent-00: …".len();
        assert!(text.len() <= DESCRIPTION.len() + 64 + MAX_LISTED * per + 30 * "agent-00, ".len(), "{} bytes", text.len());
        assert!(text.contains("- agent-19:") && !text.contains("- agent-20:"), "twenty described");
        assert!(text.ends_with(&format!("\nAlso: {}", (20..50).map(|i| format!("agent-{i:02}")).collect::<Vec<_>>().join(", "))), "the rest by name: {text}");
        assert!(!text.contains("second paragraph"));
        assert_eq!(describe(&[def(0, "Reviews diffs.\nUse after edits.".into())]), format!("{DESCRIPTION}\nAgents (name them in `agent`):\n- agent-00: Reviews diffs. Use after edits."));
        assert_eq!(describe(&[]), DESCRIPTION);
        let huge: Vec<AgentDef> = (0..1000).map(|i| AgentDef { name: format!("a{i:04}"), ..def(0, String::new()) }).collect();
        let text = describe(&huge);
        assert!(text.contains("a0199") && !text.contains("a0200") && text.ends_with(" (and 800 more)"), "named up to {MAX_NAMED}");
    }

    #[test]
    fn r_sub_1_an_allowlist_narrows_the_tools_and_never_offers_a_subagent() {
        let every = AgentRun::default();
        assert!(every.allows("bash", "str_replace") && every.allows("publish", "str_replace") && !every.allows(SUBAGENT, "str_replace"));
        let read_only = AgentRun { tools: Some(vec!["read".into(), "grep".into(), "edit".into()]), ..AgentRun::default() };
        assert!(read_only.allows("read", "apply_patch") && read_only.allows("apply_patch", "apply_patch"));
        assert!(!read_only.allows("bash", "apply_patch") && !read_only.allows("str_replace", "apply_patch") && !read_only.allows(SUBAGENT, "apply_patch"));
        let prompt = system_prompt("You are krowk.", &AgentRun { instructions: "Review diffs.".into(), ..AgentRun::default() });
        assert!(prompt.starts_with("You are krowk.\nYou are a subagent") && prompt.ends_with("\n\nReview diffs."), "{prompt}");
    }
}
