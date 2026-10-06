//! Claude-Code-compatible permissions (R-PERM-1, R-PERM-2): the modes
//! `default`, `acceptEdits`, `plan` and `bypassPermissions`, krowk's own
//! `unhinged`, the rules of
//! `permissions.allow`, `ask` and `deny` in Claude Code's syntax (`rules`),
//! the places they are read from (`settings`), grants remembered for a
//! session or a project, and approval requests any client can answer.
//!
//! One evaluator judges every call krowk is asked about: the native loop's
//! tools, and what a backend (Claude Code's `can_use_tool`, Codex's
//! approval requests) asks. The order, for a call:
//!
//! 0. **`unhinged` allows it**, and none of what follows is read: no deny
//!    rule, no fence, no ask rule, no hook's ask. The person chose to trust
//!    the model with everything this process can reach. A `PreToolUse`
//!    hook still runs, and a hook that blocks still blocks: a hook is the
//!    person's own program, not a rule.
//! 1. **A deny rule that matches denies it**, in every mode —
//!    `bypassPermissions` too. Deny always wins.
//! 2. What needs no permission (krowk's own bridged tools) runs.
//! 3. **Plan mode refuses** everything that is not reading: edits,
//!    commands, MCP tools.
//! 4. **A file tool reaching into `.git`, `.claude`, `.codex`, `.krowk` or
//!    a directory krowk keeps its own settings in is asked about**, unless
//!    permissions are bypassed — never allowed by a rule or a remembered
//!    grant, since what is written there decides what runs next (git's
//!    hooks, Claude Code's and Codex's settings, krowk's own rules).
//! 5. An ask rule that matches, or a `PreToolUse` hook that says `ask`,
//!    asks.
//! 6. `bypassPermissions` allows the rest.
//! 7. An allow rule, a grant remembered for the session or the project, or
//!    a `PreToolUse` hook that says `allow`, allows it — all of it: every
//!    path it names, every command of a command line.
//! 8. Otherwise the mode decides: reading inside the working directory and
//!    the directories the settings add (`additionalDirectories`) runs; editing there runs under
//!    `acceptEdits` and is asked about under `default`; anything reaching
//!    outside them, every command, fetch and MCP tool is asked
//!    about.
//!
//! **Asking** is an `approval.requested` frame on the session's stream,
//! answered by `Command::Approve` from whichever client is attached — the
//! TUI today, a phone through the daemon later (R-PERM-2). A host no
//! client answers for — `krowk -p` — never waits: the call is refused, and
//! the result says what to allow or which mode to rerun in.

pub mod rules;
pub mod settings;

use crate::engine::{EngineEvent, Events};
use crate::hooks;
use crate::protocol::{ApprovalDecision, ApprovalRequest, PermissionMode};
use crate::tools::{Hidden, Reach, Scope};
pub use rules::{Access, Call, Kind, Rule};
use serde_json::Value;
pub use settings::{Config, Loaded};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch};

/// What the file tools may reach and what they may open, once a call is
/// allowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opens {
    /// Paths outside the working directory and its added directories:
    /// allowed by a rule, a person or bypass.
    pub outside: bool,
    /// The fenced directories: a person approved this call, or bypass.
    pub fences: bool,
}

/// What the evaluator says of a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allow(Opens),
    /// A person decides: why, and the rules a session or project grant
    /// would remember (none when it cannot be remembered).
    Ask { reason: String, remember: Vec<String> },
    Deny(String),
}

/// Every setting that applies to one session's directory, resolved.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub loaded: Loaded,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// Directories the file tools may read, never change: skills.
    pub read_dirs: Vec<PathBuf>,
    /// Directories no file tool changes without a person's say: Claude
    /// Code's, a backend instance's own home.
    pub protected: Vec<PathBuf>,
    /// What no file tool reads, searches or changes unasked: krowk's whole
    /// home, its keys, logins, sessions and settings (`Scope::secrets`).
    pub secrets: Vec<PathBuf>,
    /// The OS sandbox the session's tools run in (R-PERM-3).
    pub sandbox: Option<crate::sandbox::Sandbox>,
    /// The turn's workspace search, shared by its calls' sandbox plans.
    pub walk: crate::sandbox::Walk,
}

impl Policy {
    /// The policy for `cwd` from `cfg`'s sources.
    pub fn load(cfg: &Config, cwd: &Path) -> Result<Policy, String> {
        let loaded = settings::load(cfg, cwd)?;
        let protected: Vec<PathBuf> = cfg.claude_home().into_iter().collect();
        // The home in use and the default one (a KROWK_HOME elsewhere does
        // not open `~/.krowk`), each with its migration staging directory
        // and lock (`krowk_api::home::siblings`), as named and, when they
        // exist, as they lead (`Scope::secret`).
        let default = cfg.home.as_ref().map(|h| krowk_api::home::lexical(h).join(".krowk"));
        let secrets = cfg.krowk_dir.iter().chain(default.iter()).flat_map(|d| krowk_api::home::siblings(d)).flat_map(|d| [d.canonicalize().ok(), Some(d)]).flatten().collect();
        Ok(Policy { loaded, cwd: cwd.to_path_buf(), home: cfg.home.clone(), read_dirs: Vec::new(), protected, secrets, sandbox: cfg.sandbox, walk: Default::default() })
    }

    /// A policy with no settings: the modes alone.
    pub fn modes_only(cwd: &Path) -> Policy {
        Policy { cwd: cwd.to_path_buf(), loaded: Loaded { root: crate::trust::root(cwd), ..Loaded::default() }, ..Policy::default() }
    }

    fn places(&self) -> rules::Places<'_> {
        rules::Places { cwd: &self.cwd, home: self.home.as_deref() }
    }

    /// The deny rules, in Claude Code's spelling: what a Claude Code
    /// backend is started with as `--disallowedTools`, so what it would
    /// allow by itself still meets them.
    pub fn deny_list(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (k, r) in &self.loaded.rules {
            if *k == Kind::Deny
                && let Some(s) = rules::claude_spelling(r)
                && !out.contains(&s)
            {
                out.push(s);
            }
        }
        out
    }

    fn scope(&self, opens: Opens) -> Scope {
        let mut scope = Scope {
            cwd: self.cwd.clone(),
            roots: self.loaded.dirs.clone(),
            read_roots: self.read_dirs.clone(),
            outside: opens.outside,
            open: opens.fences,
            protected: self.protected.clone(),
            secrets: self.secrets.clone(),
            hidden: Hidden::default(),
            sandbox: None,
        };
        if let Some(sandbox) = self.sandbox {
            let plan = crate::sandbox::Plan::new_with(sandbox, &scope.cwd, &scope.roots, &scope.read_roots, &scope.protected, &scope.secrets, self.home.as_deref(), self.walk.clone());
            // What the sandbox hides, a search skips as it skips krowk's home.
            scope.secrets.extend(plan.hidden.iter().cloned());
            scope.sandbox = Some(Arc::new(plan));
        }
        scope
    }
}

/// A short account of a call, for the person and the model.
pub fn summary(call: &Call) -> String {
    let paths = |ps: &[PathBuf]| ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    match &call.access {
        Access::Read(ps) => format!("{} {}", call.tool, paths(ps)),
        Access::Edit(ps) => format!("{} {}", call.tool, paths(ps)),
        Access::Publish(ps) => format!("publish {} to a public link", paths(ps)),
        Access::Bash(c) => format!("Bash `{}`", c.trim()),
        Access::Fetch(u) => format!("WebFetch {u}"),
        Access::Mcp { server, tool } => format!("the MCP tool {tool} of {server}"),
        Access::Skill(_) => format!("the skill {}", call.subject.as_deref().unwrap_or("?")),
        Access::Free | Access::Other | Access::Session => match &call.subject {
            Some(s) => format!("{} {s}", call.tool),
            None => call.tool.clone(),
        },
    }
}

/// The rules a session or project grant remembers for a call: the command
/// exactly, the file exactly, the directory of a file read, the host, the
/// MCP tool. None for a command line of several commands — each is its own
/// decision.
fn remember(call: &Call) -> Vec<String> {
    let abs = |p: &Path| format!("/{}", p.display());
    // A grant is a rule, and a path in a rule is glob text: a file named
    // `a[1].rs` or `*` would grant more than itself. Such a call is allowed
    // once, never remembered.
    let globby = |ps: &[PathBuf]| ps.iter().any(|p| p.to_string_lossy().contains(['*', '?', '[', ']', '{', '}']));
    if let Access::Read(ps) | Access::Edit(ps) | Access::Publish(ps) = &call.access
        && globby(ps)
    {
        return Vec::new();
    }
    if let Access::Skill(_) = &call.access
        && call.subject.as_deref().is_some_and(|s| s.contains(['*', '?', '[', ']', '{', '}', '(', ')']))
    {
        return Vec::new();
    }
    match &call.access {
        Access::Bash(c) => {
            let parsed = rules::split(c);
            match parsed.commands.as_slice() {
                [one] if !parsed.opaque && !one.writes => vec![format!("Bash({})", one.words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "))],
                _ => Vec::new(),
            }
        }
        Access::Edit(ps) => ps.iter().map(|p| format!("Edit({})", abs(p))).collect(),
        Access::Publish(ps) => ps.iter().map(|p| format!("Publish({})", abs(p))).collect(),
        // The file read, exactly; a directory's contents only when the call
        // read the directory itself.
        Access::Read(ps) => ps.iter().map(|p| if p.is_dir() { format!("Read({}/**)", abs(p).trim_end_matches('/')) } else { format!("Read({})", abs(p)) }).collect(),
        Access::Fetch(u) => url::Url::parse(u).ok().and_then(|u| u.host_str().map(|h| vec![format!("WebFetch(domain:{h})")])).unwrap_or_default(),
        Access::Mcp { server, tool } => vec![format!("mcp__{server}__{tool}")],
        Access::Skill(_) => call.subject.iter().map(|s| format!("Skill({s})")).collect(),
        Access::Free | Access::Other => vec![call.tool.clone()],
        Access::Session => vec![match &call.subject {
            Some(s) if !s.contains(['*', '?', '[', ']', '{', '}', '(', ')']) => format!("{}({s})", call.tool),
            _ => call.tool.clone(),
        }],
    }
}

/// A word as the shell would need it written to read back as one word:
/// `rm 'a b'` is remembered as that, never as `rm a b`, which removes two
/// other files.
pub(crate) fn quote(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c)) {
        return w.to_string();
    }
    format!("'{}'", w.replace('\'', "'\\''"))
}

/// Approval requests waiting for an answer, across every session of a
/// host: `Command::Approve` answers one by its id.
#[derive(Clone, Default)]
pub struct Approvals(Arc<Mutex<HashMap<String, Waiting>>>);

struct Waiting {
    session_id: String,
    answer: oneshot::Sender<ApprovalDecision>,
}

impl Approvals {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Waiting>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait(&self, id: &str, session_id: &str) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id.to_string(), Waiting { session_id: session_id.to_string(), answer: tx });
        rx
    }

    fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Answers the request `id` of `session_id`.
    pub fn answer(&self, session_id: &str, id: &str, decision: ApprovalDecision) -> Result<(), String> {
        let mut map = self.lock();
        match map.get(id) {
            Some(w) if w.session_id == session_id => {}
            _ => return Err(format!("session {session_id} has no approval request {id} waiting — it was answered, or its turn is over")),
        }
        let w = map.remove(id).expect("checked above");
        w.answer.send(decision).map_err(|_| format!("the request {id} is no longer waiting"))
    }

    /// Answers every request of `session_id` still waiting with `deny`:
    /// nobody is left to answer them (the daemon's last client of the
    /// session went away).
    pub fn deny_session(&self, session_id: &str) {
        let mut map = self.lock();
        let ids: Vec<String> = map.iter().filter(|(_, w)| w.session_id == session_id).map(|(id, _)| id.clone()).collect();
        for id in ids {
            if let Some(w) = map.remove(&id) {
                let _ = w.answer.send(ApprovalDecision::Deny);
            }
        }
    }

    /// Drops whatever a session's finished turn left waiting.
    pub fn forget_session(&self, session_id: &str) {
        self.lock().retain(|_, w| w.session_id != session_id);
    }
}

/// Grants a person gave for the rest of a session.
pub type SessionGrants = Arc<Mutex<Vec<Rule>>>;

/// One turn's permissions: the policy, the mode, the session's grants, and
/// who answers when a call is asked about.
#[derive(Clone)]
pub struct Gate(Arc<Inner>);

struct Inner {
    policy: Policy,
    mode: PermissionMode,
    grants: SessionGrants,
    approvals: Option<Approvals>,
    grants_file: Option<PathBuf>,
    session_id: String,
    turn_id: String,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").field("mode", &self.0.mode).field("cwd", &self.0.policy.cwd).field("asks", &self.0.approvals.is_some()).finish()
    }
}

impl Gate {
    pub fn new(policy: Policy, mode: PermissionMode, grants: SessionGrants, approvals: Option<Approvals>, grants_file: Option<PathBuf>, session_id: &str, turn_id: &str) -> Gate {
        Gate(Arc::new(Inner { policy, mode, grants, approvals, grants_file, session_id: session_id.into(), turn_id: turn_id.into() }))
    }

    /// The modes alone, in `cwd`, with nobody to ask: what a caller that
    /// was handed no settings judges by.
    pub fn modes_only(cwd: &Path, mode: PermissionMode) -> Gate {
        Gate::new(Policy::modes_only(cwd), mode, SessionGrants::default(), None, None, "", "")
    }

    pub fn mode(&self) -> PermissionMode {
        self.0.mode
    }

    pub fn policy(&self) -> &Policy {
        &self.0.policy
    }

    /// The same gate in another mode.
    fn in_mode(&self, mode: PermissionMode) -> Gate {
        Gate(Arc::new(Inner { policy: self.0.policy.clone(), mode, grants: self.0.grants.clone(), approvals: None, grants_file: None, session_id: self.0.session_id.clone(), turn_id: self.0.turn_id.clone() }))
    }

    /// The same gate, with more directories no file tool changes unasked: a
    /// backend instance's own config directory.
    pub fn protecting(&self, dirs: impl IntoIterator<Item = PathBuf>) -> Gate {
        let mut policy = self.0.policy.clone();
        policy.protected.extend(dirs);
        Gate(Arc::new(Inner { policy, mode: self.0.mode, grants: self.0.grants.clone(), approvals: self.0.approvals.clone(), grants_file: self.0.grants_file.clone(), session_id: self.0.session_id.clone(), turn_id: self.0.turn_id.clone() }))
    }

    /// The scope a call's file work runs in once it is allowed: the
    /// working directory and its added directories, the skills to read,
    /// what the verdict opened, and the
    /// files a deny rule keeps from being read — which a search skips.
    pub fn scope(&self, opens: Opens) -> Scope {
        let mut s = self.0.policy.scope(opens);
        let (me, secrets) = (self.clone(), Scope { secrets: s.secrets.clone(), ..Scope::within(&s.cwd) });
        // A search skips krowk's home as it skips what a deny rule hides —
        // except under unhinged, which no fence and no rule holds.
        if self.0.mode != PermissionMode::Unhinged && (!s.secrets.is_empty() || self.0.policy.loaded.rules.iter().any(|(k, _)| *k == Kind::Deny)) {
            s.hidden = Hidden(Some(Arc::new(move |p: &Path| secrets.secret(p) || me.denies_read(p))));
        }
        s
    }

    /// Whether a deny rule keeps `p` from being read.
    pub fn denies_read(&self, p: &Path) -> bool {
        let call = Call { tool: "Read".into(), access: Access::Read(vec![p.to_path_buf()]), subject: None };
        let at = self.0.policy.places();
        self.0.policy.loaded.rules.iter().any(|(k, r)| *k == Kind::Deny && rules::matches(r, &call, &at, false))
    }

    /// Where a call's paths lead: those outside the tools' reach, and the
    /// first fenced one's reason.
    fn reach(&self, call: &Call) -> (Vec<PathBuf>, Option<String>) {
        let (paths, edit) = match &call.access {
            Access::Read(ps) | Access::Publish(ps) => (ps, false),
            Access::Edit(ps) => (ps, true),
            _ => return (Vec::new(), None),
        };
        let scope = self.0.policy.scope(Opens::default());
        let mut outside = Vec::new();
        let mut fenced = None;
        for p in paths {
            match scope.reach(p, edit) {
                Reach::Inside => {}
                Reach::Outside(_) => outside.push(p.clone()),
                Reach::Fenced(why) => {
                    fenced.get_or_insert(why);
                }
            }
        }
        (outside, fenced)
    }

    fn allowed(&self, call: &Call) -> bool {
        let at = self.0.policy.places();
        let grants = self.0.grants.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let allow: Vec<&Rule> = self.0.policy.loaded.rules.iter().filter(|(k, _)| *k == Kind::Allow).map(|(_, r)| r).chain(grants.iter()).collect();
        match &call.access {
            Access::Bash(cmd) => rules::bash_covered(&allow, cmd, &self.0.policy.cwd),
            Access::Read(ps) | Access::Edit(ps) | Access::Publish(ps) => {
                !ps.is_empty() && ps.iter().all(|p| {
                    let one = Call {
                        access: match call.access {
                            Access::Edit(_) => Access::Edit(vec![p.clone()]),
                            Access::Publish(_) => Access::Publish(vec![p.clone()]),
                            _ => Access::Read(vec![p.clone()]),
                        },
                        ..call.clone()
                    };
                    allow.iter().any(|r| rules::matches(r, &one, &at, true))
                })
            }
            _ => allow.iter().any(|r| rules::matches(r, call, &at, true)),
        }
    }

    /// The evaluator itself (see the module's notes for the order).
    pub fn verdict(&self, call: &Call, hook: Option<hooks::Decision>) -> Verdict {
        if self.0.mode == PermissionMode::Unhinged {
            return Verdict::Allow(Opens { outside: true, fences: true });
        }
        let p = &self.0.policy;
        let at = p.places();
        let what = summary(call);
        if let Some((_, r)) = p.loaded.rules.iter().find(|(k, r)| *k == Kind::Deny && rules::matches(r, call, &at, false)) {
            return Verdict::Deny(format!("{what} is denied by the rule `{}` in {}. Do not look for another way to do it; say what you needed it for instead.", r.text, r.source));
        }
        if call.access == Access::Free {
            return Verdict::Allow(Opens::default());
        }
        // krowk's own session tools: no mode, plan's included, holds them,
        // but an ask rule or a hook's ask does — in bypassPermissions too,
        // as for any call.
        if call.access == Access::Session {
            if let Some((_, r)) = p.loaded.rules.iter().find(|(k, r)| *k == Kind::Ask && rules::matches(r, call, &at, false)) {
                return Verdict::Ask { reason: format!("the rule `{}` in {} asks first", r.text, r.source), remember: remember(call) };
            }
            if hook == Some(hooks::Decision::Ask) {
                return Verdict::Ask { reason: "a PreToolUse hook asks for it to be approved".into(), remember: Vec::new() };
            }
            return Verdict::Allow(Opens::default());
        }
        let mode = self.0.mode;
        if mode == PermissionMode::Plan && !matches!(call.access, Access::Read(_) | Access::Fetch(_) | Access::Skill(_)) {
            return Verdict::Deny(format!("{what} is not run in plan mode, which reads and plans but changes nothing. Say what you would do instead; the person leaves plan mode to have it done."));
        }
        let bypass = mode == PermissionMode::BypassPermissions;
        let (outside, fenced) = self.reach(call);
        if !bypass && let Some(why) = fenced {
            return Verdict::Ask { reason: why, remember: Vec::new() };
        }
        if let Some((_, r)) = p.loaded.rules.iter().find(|(k, r)| *k == Kind::Ask && rules::matches(r, call, &at, false)) {
            return Verdict::Ask { reason: format!("the rule `{}` in {} asks first", r.text, r.source), remember: remember(call) };
        }
        if hook == Some(hooks::Decision::Ask) {
            return Verdict::Ask { reason: "a PreToolUse hook asks for it to be approved".into(), remember: Vec::new() };
        }
        // A command line whose program the shell computes, or that krowk
        // cannot follow, cannot be held to a deny rule: where one applies,
        // it is asked about — in bypassPermissions too, and refused where
        // nobody can be asked.
        if let Access::Bash(cmd) = &call.access
            && p.loaded.rules.iter().any(|(k, r)| *k == Kind::Deny && r.tool == "Bash")
            && rules::split(cmd).opaque
        {
            return Verdict::Ask { reason: "krowk cannot tell which program it runs (a name the shell expands, a quote it cannot follow), so a deny rule could not hold it".into(), remember: Vec::new() };
        }
        if bypass {
            return Verdict::Allow(Opens { outside: true, fences: true });
        }
        if hook == Some(hooks::Decision::Allow) || self.allowed(call) {
            return Verdict::Allow(Opens { outside: !outside.is_empty(), fences: false });
        }
        let out = |ps: &[PathBuf]| ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
        let reason = match &call.access {
            Access::Read(_) if outside.is_empty() => return Verdict::Allow(Opens::default()),
            Access::Read(_) => format!("it reads outside the working directory {} ({})", p.cwd.display(), out(&outside)),
            Access::Edit(_) if !outside.is_empty() => format!("it changes files outside the working directory {} ({})", p.cwd.display(), out(&outside)),
            Access::Edit(_) if mode == PermissionMode::AcceptEdits => return Verdict::Allow(Opens::default()),
            Access::Edit(_) => "it changes files, and this session is in the default mode, which asks before every edit".into(),
            Access::Publish(_) if !outside.is_empty() => format!("it publishes files from outside the working directory {} ({})", p.cwd.display(), out(&outside)),
            Access::Publish(_) if mode == PermissionMode::AcceptEdits => return Verdict::Allow(Opens::default()),
            Access::Publish(_) => "it uploads files to a link anyone can open, and this session is in the default mode, which asks first".into(),
            Access::Bash(_) => "it runs a command, and no allow rule covers it".into(),
            Access::Fetch(_) => "it fetches from the network, and no allow rule covers the host".into(),
            Access::Mcp { .. } => "it calls an MCP tool no allow rule covers".into(),
            // Loading a skill reads its own file: what reading needs.
            Access::Skill(_) => return Verdict::Allow(Opens::default()),
            Access::Free | Access::Other | Access::Session => format!("no allow rule covers {}", call.tool),
        };
        Verdict::Ask { reason, remember: remember(call) }
    }

    /// Judges a call and, when it is asked about, asks: through the host's
    /// approvals when a client answers them, else refused with what would
    /// allow it. `tool` and `input` are the call as the model made it, for
    /// the person to see. Returns what the allowed call may reach, or the
    /// refusal the model reads.
    pub async fn check(&self, call: &Call, tool: &str, input: &Value, hook: Option<hooks::Decision>, events: &Events, cancel: &watch::Receiver<bool>) -> Result<Opens, String> {
        let (reason, remember) = match self.verdict(call, hook) {
            Verdict::Allow(o) => return Ok(o),
            Verdict::Deny(m) => return Err(m),
            Verdict::Ask { reason, remember } => (reason, remember),
        };
        let what = summary(call);
        let Some(approvals) = &self.0.approvals else { return Err(self.nobody_to_ask(call, hook, &what, &reason, &remember)) };
        let request_id = krowk_store::new_id();
        let answer = approvals.wait(&request_id, &self.0.session_id);
        let req = ApprovalRequest {
            session_id: self.0.session_id.clone(),
            turn_id: self.0.turn_id.clone(),
            request_id: request_id.clone(),
            tool: tool.to_string(),
            input: input.clone(),
            summary: what.clone(),
            reason: reason.clone(),
            remember: remember.clone(),
        };
        let _ = events.send(EngineEvent::Approval(req)).await;
        let mut cancel = cancel.clone();
        let decision = tokio::select! {
            d = answer => d.unwrap_or(ApprovalDecision::Deny),
            _ = crate::engine::cancelled(&mut cancel) => ApprovalDecision::Deny,
        };
        approvals.forget(&request_id);
        let _ = events.send(EngineEvent::ApprovalResolved { request_id, decision }).await;
        let grant = |rules: &[String]| {
            let mut g = self.0.grants.lock().unwrap_or_else(|e| e.into_inner());
            for r in rules {
                if let Ok(rule) = rules::parse(r, "a grant for this session", &self.0.policy.loaded.root) {
                    g.push(rule);
                }
            }
        };
        match decision {
            ApprovalDecision::Allow => {}
            ApprovalDecision::AllowSession => grant(&remember),
            ApprovalDecision::AllowProject => {
                grant(&remember);
                if let Some(file) = &self.0.grants_file
                    && !remember.is_empty()
                {
                    // Allowed either way; a file that cannot be written only
                    // means it is asked again next session.
                    let _ = settings::remember(file, &self.0.policy.loaded.root, &remember);
                }
            }
            ApprovalDecision::Deny if *cancel.borrow() => return Err(format!("{what} was not run: the turn was interrupted while it waited for approval")),
            ApprovalDecision::Deny => return Err(format!("the person declined {what}. Do not try another way around it; ask what they would like instead.")),
        }
        Ok(Opens { outside: true, fences: true })
    }

    /// The refusal for a call that would be asked about when nobody can
    /// answer: what it needed, and what would allow it.
    fn nobody_to_ask(&self, call: &Call, hook: Option<hooks::Decision>, what: &str, reason: &str, remember: &[String]) -> String {
        // A session tool is asked about only by an ask rule or a hook, which
        // no allow rule and no mode but unhinged gets past.
        if call.access == Access::Session {
            return format!("{what} needs approval — {reason} — and nobody is here to give it: this session cannot ask. Run it where someone can answer (bare `krowk`), remove what asks, or rerun with `--permission-mode unhinged`.");
        }
        // What bypassPermissions would still ask about — an ask rule, a
        // hook's ask, a line a deny rule could not see into — only unhinged
        // runs.
        let past_bypass = matches!(self.in_mode(PermissionMode::BypassPermissions).verdict(call, hook), Verdict::Ask { .. });
        let at = self.0.policy.places();
        let untrusted = self.0.policy.loaded.ignored_allow.iter().find(|r| rules::matches(r, call, &at, true));
        let (outside, fenced) = self.reach(call);
        let mode = match &call.access {
            _ if past_bypass => "unhinged",
            Access::Edit(_) | Access::Publish(_) if outside.is_empty() && fenced.is_none() => "acceptEdits",
            _ => "bypassPermissions",
        };
        // No allow rule gets past what only unhinged runs.
        let rule = remember.first().filter(|_| !past_bypass).map(|r| format!("an allow rule such as `{r}` in `permissions.allow` of .claude/settings.json or krowk's config.json, or ")).unwrap_or_default();
        let hint = match untrusted.filter(|_| !past_bypass) {
            Some(r) => format!(" ({} allows it with `{}`, but a repository's own allow rules apply only once it is trusted — run krowk there once on a terminal and answer its trust question, or pass --trust.)", r.source, r.text),
            None => String::new(),
        };
        format!("{what} needs approval — {reason} — and nobody is here to give it: this session cannot ask. It is allowed by {rule}rerunning with `--permission-mode {mode}`.{hint}")
    }
}

/// A call judged by the mode alone, as a result: what the backends' unit
/// tests check their mapping against.
#[cfg(test)]
pub(crate) fn judge(mode: PermissionMode, call: Result<Call, String>, cwd: &Path, protected: &[PathBuf]) -> Result<(), String> {
    match Gate::modes_only(cwd, mode).protecting(protected.to_vec()).verdict(&call?, None) {
        Verdict::Allow(_) => Ok(()),
        Verdict::Deny(m) => Err(m),
        Verdict::Ask { reason, .. } => Err(format!("asked: {reason}")),
    }
}

#[cfg(test)]
mod tests;
