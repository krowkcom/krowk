//! MCP servers a native turn can use, with their tools deferred (R-TOOL-3).
//!
//! A person's MCP servers come from the places Claude Code keeps them, so a
//! setup made for it needs nothing new. Lowest first — a later source's
//! server replaces an earlier one of the same name:
//!
//! | source | file | read |
//! |---|---|---|
//! | project | `<repository>/.mcp.json`: `mcpServers` | only once the repository is trusted, and then those Claude Code's `enabledMcpjsonServers`, `disabledMcpjsonServers` and `enableAllProjectMcpServers` leave on |
//! | Claude Code, user | Claude's `settings.json`, then `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json`): `mcpServers` | always |
//! | Claude Code, local | `~/.claude.json`: `projects["<working directory>"].mcpServers`, else `projects["<repository>"]`'s | always |
//! | krowk, user | `config.json` in krowk's home: `mcpServers` | always |
//!
//! The project is lowest, where Claude Code puts it above the user's: a
//! repository's `github` must not take the place of the person's, whose
//! allow rules name it. Each server is Claude Code's shape: `{command,
//! args, env}` for a stdio server, `{type: "http", url, headers}` for a
//! streamable-HTTP one, with `${VAR}` and `${VAR:-default}` expanded from
//! the environment. A server whose name or config cannot be used is left
//! out and named, with why, in a notice of the native turn.
//!
//! **Trust.** A project's `.mcp.json` is a command someone else wrote, so it
//! is neither started nor listed until the person answers the trust
//! question for the repository (R-BACK-6's, the same list). The person's own
//! servers start in the working directory only when the repository is
//! trusted, and in their home otherwise: `npx` runs a repository's
//! `node_modules/.bin` first and `python -m` imports from the working
//! directory, so an untrusted repository would otherwise choose the code
//! the person's own server runs.
//!
//! **Deferred.** However many servers and tools there are, the model is
//! offered two tools: `mcp_search`, which finds tools by what they do and
//! returns only the matches' schemas, and `mcp_call`, which calls one by its
//! `server:tool` name. So fifty MCP tools cost the context what two small
//! definitions cost, until one is used. No server is started before the
//! first of those calls, and none a deny rule covers whole.
//!
//! Every call is judged as `mcp__server__tool` (`Mcp(server:tool)` in a
//! rule) by the one permission evaluator; a tool a deny rule covers is left
//! out of search results too, as Claude Code leaves it out of the tool list.
//! A subagent whose definition names MCP servers or tools reaches those
//! alone (`crate::subagent::AgentRun::reaches`): the rest are left out of its
//! search, its calls to them refused, and a server it cannot reach is not
//! started for it.
//!
//! Each server starts on its own, once a turn first needs it: the session
//! and its subagents share the servers, and a subagent that reaches one
//! server must neither start the others nor leave them unstarted for its
//! parent.
//!
//! **Across turns.** A session's servers stay connected (`Pool`): each
//! turn re-reads the configuration and adopts it, keeping a server whose
//! configuration and working directory are as they were, restarting one
//! that changed, and closing one that is gone, untrusted or whose
//! `.mcp.json` no longer matches its digest, before the turn's first model
//! call. The host closes the pool with the session's idle sweep and at
//! shutdown.
//!
//! **Status.** Each server is `connecting`, `connected`, `needs sign-in` (an
//! HTTP server that answered 401) or `failed` with a reason (`Pool::status`).
//! A turn starts its servers in the background (`Mcp::warm`) rather than at
//! its first call, which waits for one still connecting. A failed server,
//! or a stdio server whose process exited, is started again by the next call
//! that needs it, but not within `RETRY_AFTER` of its last start, or at once
//! by `Pool::reconnect`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

pub const SEARCH: &str = "mcp_search";
pub const CALL: &str = "mcp_call";

/// The MCP revision krowk speaks, as the bridge does.
const MCP_VERSION: &str = "2025-06-18";
/// A server that has not answered `initialize` and `tools/list` by then is
/// reported as down, so one slow server cannot hold every search.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// A failed server is not started again by a call sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long ending an HTTP session may take: a server that does not answer
/// is let go of.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one tool call may run.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Search returns at most this many tools, schemas and all.
const MATCHES: usize = 5;
/// A tool's output beyond this is cut, as bash's is.
const MAX_OUTPUT: usize = 30_000;
/// One message from a server may be at most this long: a server that
/// never ends a line cannot fill the memory.
const MAX_MESSAGE: u64 = 16 << 20;
/// What a stdio server may write that is not a JSON-RPC message while one
/// request waits, in lines and in bytes: a few lines of a banner or of
/// logging pass, a server writing nothing else is stopped long before
/// `CALL_TIMEOUT`. Its notifications are messages, and do not count.
const MAX_JUNK_LINES: usize = 1000;
const MAX_JUNK_BYTES: usize = 1 << 20;
/// What one server's `tools/list` may add up to, over at most this many
/// pages: the list is held for the session, and a server that pages
/// forever must not hold the turn until `START_TIMEOUT`.
const MAX_LISTED: usize = 4 << 20;
const MAX_PAGES: usize = 100;
/// A tool whose input schema is bigger than this is left out: search hands
/// the model its schema whole, and one tool must not fill the context.
const MAX_SCHEMA: usize = 16 << 10;
/// All of one search result, and the list of names a search with no match
/// answers with.
const MAX_SEARCH: usize = 32 << 10;
const MAX_NAMES: usize = 4 << 10;
/// A `.mcp.json` bigger than this is not read.
const MAX_MCP_JSON: u64 = 1 << 20;
/// krowk's own credentials, which a stdio server does not inherit unless
/// its config sets them in `env`: a server is someone else's program, and
/// the person's provider keys are not its to spend.
const SCRUBBED: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "XAI_API_KEY", "OPENROUTER_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"];

/// One server's configuration, as `.mcp.json` writes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerConfig {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// A server, named, with the file it came from and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub config: ServerConfig,
    pub source: String,
    pub cwd: PathBuf,
}

/// Whether a server name is one rules can name unambiguously: `mcp__a__b`
/// must read one way, so no `__`, no `_` at either end, and nothing a rule
/// treats as syntax.
pub fn good_name(n: &str) -> bool {
    !n.is_empty() && !n.contains("__") && !n.starts_with('_') && !n.ends_with('_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl ServerConfig {
    /// How it connects: `stdio`, `http`, `sse` (named, not spoken), or
    /// `invalid` when it names neither a command nor a url.
    pub fn transport(&self) -> &'static str {
        match (self.kind.as_deref(), &self.command, &self.url) {
            (Some("sse"), ..) => "sse",
            (_, Some(_), _) => "stdio",
            (_, None, Some(_)) => "http",
            _ => "invalid",
        }
    }
}

/// The servers a repository's `.mcp.json` names that `discover` leaves out
/// because the repository is not trusted, or its file changed since it was:
/// what `krowk mcp list` shows as untrusted. None once it is trusted.
pub fn untrusted_project(cfg: &crate::permissions::Config, cwd: &Path) -> Vec<(String, ServerConfig)> {
    let root = crate::trust::root(cwd);
    if cfg.trusted.as_ref().is_some_and(|t| t(&root)) {
        return Vec::new();
    }
    let Some(Value::Object(m)) = read_project(&root).and_then(|v| v.get("mcpServers").cloned()) else { return Vec::new() };
    m.into_iter().filter(|(n, _)| good_name(n)).filter_map(|(n, c)| Some((n, ServerConfig::deserialize(c).ok()?))).collect()
}

/// Every server that applies in `cwd`, lowest source first, later names
/// replacing earlier ones, and why each server left out was: a name a rule
/// could not name one way, or a config that is not a server's. A project's
/// `.mcp.json` counts only when its repository is trusted, and then only
/// the servers Claude Code's own settings leave on (`Approval`).
pub fn discover(cfg: &crate::permissions::Config, cwd: &Path) -> (Vec<Server>, Vec<String>) {
    let root = crate::trust::root(cwd);
    let trusted = cfg.trusted.as_ref().is_some_and(|t| t(&root));
    // Untrusted and with no home, there is nowhere safe to start a stdio
    // server: a shared temporary directory is anyone's to plant in.
    let runs_in = if trusted { Some(cwd.to_path_buf()) } else { cfg.home.clone() };
    let mut out: Vec<Server> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut add = |v: Option<&Value>, source: &str, on: &dyn Fn(&str) -> bool| {
        let Some(Value::Object(m)) = v else { return };
        for (name, c) in m {
            if !good_name(name) {
                skipped.push(format!("the MCP server {name:?} in {source} was skipped: a name may hold only letters, digits, - and _, with no __ and no _ at either end, so mcp__server__tool reads one way in a rule"));
                continue;
            }
            let config = match ServerConfig::deserialize(c) {
                Ok(c) => c,
                Err(e) => {
                    skipped.push(format!("the MCP server {name} in {source} was skipped: {}", crate::http::clip(&unquoted(&e.to_string()), 200)));
                    continue;
                }
            };
            if !on(name) {
                continue;
            }
            let Some(runs_in) = runs_in.clone().or_else(|| config.command.is_none().then(PathBuf::new)) else { continue };
            out.retain(|s| s.name != *name);
            out.push(Server { name: name.clone(), config: expand(config), source: source.to_string(), cwd: runs_in });
        }
    };
    let read = |p: &Path| std::fs::read(p).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    let claude_settings = cfg.claude_home().and_then(|dir| read(&dir.join("settings.json")));
    let claude_json = match &cfg.claude_dir {
        Some(d) => Some(d.join(".claude.json")),
        None => cfg.home.as_ref().map(|h| h.join(".claude.json")),
    };
    let claude_json = claude_json.as_deref().and_then(read);
    // Claude Code keys a project by the directory it was started in, so
    // that comes first; the repository's root is what krowk keyed it by.
    let project = claude_json.as_ref().and_then(|v| v.get("projects")).and_then(|p| [cwd, root.as_path()].iter().find_map(|k| p.get(k.display().to_string())));
    // Read once: the servers loaded are the ones whose digest is checked
    // here against what trust recorded, never a file swapped in after the
    // check (`trusted` read it too, but a moment earlier).
    if trusted && let Some(v) = read_project(&root) {
        let recorded = cfg.krowk_dir.as_ref().and_then(|d| crate::trust::Store::new(Some(d.join(crate::trust::FILE)), cfg.home.clone()).recorded_mcp(&root));
        // Not listed at all: trusted for this run (`--trust`, or a yes not
        // yet remembered), which covers what is there now.
        if recorded.is_none_or(|r| r == digest(&v)) {
            let approval = Approval::read(claude_settings.as_ref(), project, &root, cwd);
            add(v.get("mcpServers"), ".mcp.json", &|n| approval.on(n));
        }
    }
    let all = |_: &str| true;
    add(claude_settings.as_ref().and_then(|s| s.get("mcpServers")), "Claude Code's settings.json", &all);
    add(claude_json.as_ref().and_then(|v| v.get("mcpServers")), "~/.claude.json", &all);
    add(project.and_then(|p| p.get("mcpServers")), "~/.claude.json (this project)", &all);
    let user = match (&cfg.user_path, cfg.reread) {
        (Some(p), true) => read(p),
        _ => cfg.user.clone(),
    };
    add(user.as_ref().and_then(|u| u.get("mcpServers")), "krowk's config.json", &all);
    (out, skipped)
}

/// Which of a project's `.mcp.json` servers Claude Code's settings leave
/// on: `disabledMcpjsonServers` turns one off whatever else says; once
/// `enabledMcpjsonServers` is set, only the servers it names are on, unless
/// `enableAllProjectMcpServers` is true. They are read where Claude Code
/// reads them — its user `settings.json`, this project's entry in
/// `~/.claude.json` (where its approval dialog writes), and the
/// repository's `.claude/settings.json` and `.claude/settings.local.json`
/// from its root down to the working directory — the lists joined, the
/// most specific `enableAllProjectMcpServers` standing. Trust is still the
/// ceiling: none of them turns on a server the trust question did not
/// cover, so a repository's own say here can only narrow what was trusted.
#[derive(Debug, Default)]
struct Approval {
    enabled: Option<Vec<String>>,
    disabled: Vec<String>,
    all: Option<bool>,
}

impl Approval {
    fn read(user: Option<&Value>, project: Option<&Value>, root: &Path, cwd: &Path) -> Approval {
        let mut a = Approval::default();
        let mut take = |v: &Value| {
            let names = |k: &str| v.get(k).and_then(|l| l.as_array()).map(|l| l.iter().filter_map(|n| n.as_str().map(String::from)).collect::<Vec<_>>());
            // Claude Code writes `[]` into every project it has opened: an
            // empty list is no list, or it would turn every server off.
            if let Some(on) = names("enabledMcpjsonServers").filter(|l| !l.is_empty()) {
                a.enabled.get_or_insert_with(Vec::new).extend(on);
            }
            a.disabled.extend(names("disabledMcpjsonServers").unwrap_or_default());
            if let Some(all) = v.get("enableAllProjectMcpServers").and_then(|b| b.as_bool()) {
                a.all = Some(all);
            }
        };
        user.into_iter().chain(project).for_each(&mut take);
        for dir in crate::permissions::settings::chain(root, cwd) {
            for name in ["settings.json", "settings.local.json"] {
                let p = dir.join(".claude").join(name);
                if let Some(v) = crate::permissions::settings::read_plain(&p, MAX_MCP_JSON, true).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()) {
                    take(&v);
                }
            }
        }
        a
    }

    fn on(&self, name: &str) -> bool {
        let named = |l: &[String]| l.iter().any(|n| n == name);
        !named(&self.disabled) && (self.all == Some(true) || self.enabled.as_deref().is_none_or(named))
    }
}

/// A config error with what it quoted taken out: serde names the value it
/// could not read, and a server's `env` or `headers` may hold a secret.
/// Everything from the first quote to the last goes, so a quote escaped
/// inside the value cannot put part of it outside.
fn unquoted(e: &str) -> String {
    match (e.find('"'), e.rfind('"')) {
        (Some(a), Some(b)) if a < b => format!("{}\"…\"{}", &e[..a], &e[b + 1..]),
        _ => e.to_string(),
    }
}

/// Whether a repository's `.mcp.json` names a server: one of the things
/// trust would turn on.
pub fn project_has_servers(root: &Path) -> bool {
    project_digest(root).is_some()
}

/// A repository's `.mcp.json`, read only when it is a plain file of a
/// sane size: it is read before the trust question, so a FIFO or a device
/// there must not hang the harness.
fn read_project(root: &Path) -> Option<Value> {
    serde_json::from_slice(&crate::permissions::settings::read_plain(&root.join(".mcp.json"), MAX_MCP_JSON, false).ok()?).ok()
}

/// A digest of the servers a repository's `.mcp.json` names, none when it
/// names none: what trust is recorded against (`crate::trust`).
pub fn project_digest(root: &Path) -> Option<String> {
    digest(&read_project(root)?)
}

/// The digest of the servers a parsed `.mcp.json` names, none when it names
/// none.
fn digest(v: &Value) -> Option<String> {
    use sha2::Digest;
    let servers = v.get("mcpServers").filter(|m| m.as_object().is_some_and(|m| !m.is_empty()))?.to_string();
    Some(sha2::Sha256::digest(servers.as_bytes()).iter().map(|b| format!("{b:02x}")).collect())
}

fn expand(mut c: ServerConfig) -> ServerConfig {
    let get = |k: &str| std::env::var(k).ok();
    let e = |s: &mut String| *s = expand_vars(s, &get);
    if let Some(s) = c.command.as_mut() {
        e(s);
    }
    if let Some(s) = c.url.as_mut() {
        e(s);
    }
    c.args.iter_mut().for_each(e);
    c.env.values_mut().for_each(e);
    c.headers.values_mut().for_each(e);
    c
}

/// `${VAR}` and `${VAR:-default}`, as Claude Code expands them; a variable
/// that is not set, with no default, expands to nothing.
fn expand_vars(s: &str, get: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let Some(end) = rest[at..].find('}') else {
            out.push_str(&rest[at..]);
            return out;
        };
        let inner = &rest[at + 2..at + end];
        let (k, default) = inner.split_once(":-").map_or((inner, None), |(k, d)| (k, Some(d)));
        out.push_str(&get(k).filter(|v| !v.is_empty() || default.is_none()).or(default.map(String::from)).unwrap_or_default());
        rest = &rest[at + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A tool one server lists.
#[derive(Debug, Clone)]
struct Tool {
    name: String,
    description: String,
    schema: Value,
}

/// One server once started: its connection and what it listed, or why it
/// is not running, and what of its list was left out.
struct Live {
    name: String,
    conn: Option<Conn>,
    tools: Vec<Tool>,
    error: Option<String>,
    left_out: Option<String>,
    /// When it finished starting, well or not: a retry waits from here.
    at: std::time::Instant,
    /// It answered 401: it wants a sign-in.
    signin: bool,
}

/// What state a server is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    NeedsSignIn,
    Failed(String),
}

/// One server as the host reports it.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    /// Where it is configured (`Server::source`).
    pub source: String,
    pub status: Status,
    /// Its tools, as `(name, description)`; none until it has connected.
    pub tools: Vec<(String, String)>,
}

impl ServerStatus {
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

/// Whether a deny rule, or a subagent's allowlist, keeps the turn from
/// `server`'s `tool` — or, `tool` none, from the whole server: a server
/// whole is its own question, never a tool's name, so no tool a server
/// lists can stand for all of it.
pub type Denied<'a> = &'a (dyn Fn(&str, Option<&str>) -> bool + Sync);

/// The session's MCP servers: configured up front, each started on its
/// first use. Once started they are read without a lock, so one long call
/// never holds another, or a search, behind it.
pub struct Mcp {
    servers: Vec<Server>,
    /// One a server, in `servers`' order.
    live: Vec<Cell>,
    retry: Duration,
    /// The credentials file the servers' sign-ins live in; none, none used.
    credentials: Option<PathBuf>,
}

/// One server's connection, set once it has started. Shared with the
/// session's `Pool`, which keeps it for the next turn.
type Cell = std::sync::Arc<Slot>;

/// The current start of one server: replaced by a fresh one to retry.
#[derive(Default)]
struct Slot(std::sync::Mutex<std::sync::Arc<tokio::sync::OnceCell<Live>>>);

type Started = std::sync::Arc<tokio::sync::OnceCell<Live>>;

impl Slot {
    /// The current start; `retry` replaces one that failed or whose
    /// connection ended, `after` or longer ago, with a fresh one.
    fn current(&self, retry: Option<Duration>) -> Started {
        let mut cur = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(after) = retry
            && cur.get().is_some_and(|l| l.dead() && l.at.elapsed() >= after)
        {
            *cur = Started::default();
        }
        cur.clone()
    }

    fn reset(&self) -> Started {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Starts `s` into `cell` unless it was.
async fn boot<'a>(cell: &'a tokio::sync::OnceCell<Live>, s: &Server, credentials: Option<&Path>) -> &'a Live {
    cell.get_or_init(|| async {
        let (conn, (tools, left_out), error) = match tokio::time::timeout(START_TIMEOUT, start(s, credentials)).await {
            Ok(Ok((conn, listed))) => (Some(conn), listed, None),
            Ok(Err(e)) => (None, Default::default(), Some(e)),
            Err(_) => (None, Default::default(), Some(format!("it did not start within {}s", START_TIMEOUT.as_secs()))),
        };
        let signin = error.as_deref().is_some_and(|e| e.starts_with("HTTP 401"));
        Live { name: s.name.clone(), conn, tools, error, left_out, at: std::time::Instant::now(), signin }
    })
    .await
}

/// A session's MCP connections, kept open across its turns (D5). Each turn
/// `adopt`s the servers it just discovered.
pub struct Pool {
    kept: std::sync::Mutex<Vec<(Server, Cell)>>,
    retry: Duration,
    credentials: Option<PathBuf>,
    /// A sign-in begun from the TUI (`sign_in_start`), per server, until it
    /// finishes, is replaced, or expires.
    signins: std::sync::Mutex<std::collections::HashMap<String, SignIn>>,
    signin_ttl: Duration,
}

/// A sign-in begun from the TUI: what it waits on, and since when.
struct SignIn {
    at: std::time::Instant,
    wait: SignInWait,
}

enum SignInWait {
    /// The host listens for the browser's redirect itself.
    Loopback(#[allow(dead_code)] Abort),
    /// The person pastes the address the browser landed on.
    Paste(Box<crate::mcp_oauth::Pending>),
}

/// A task that ends with its handle: an abandoned sign-in stops listening.
struct Abort(tokio::task::JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Default for Pool {
    fn default() -> Pool {
        Pool::with_retry(RETRY_AFTER)
    }
}

impl Pool {
    /// A pool that retries a failed server no sooner than `retry` after its
    /// last start: tests shorten it.
    pub fn with_retry(retry: Duration) -> Pool {
        Pool { kept: Default::default(), retry, credentials: None, signins: Default::default(), signin_ttl: crate::oauth::LOGIN_TIMEOUT }
    }

    /// Remote servers send the sign-ins `krowk mcp login` stored in `path`.
    pub fn with_credentials(mut self, path: PathBuf) -> Pool {
        self.credentials = Some(path);
        self
    }

    /// A sign-in that goes unanswered this long expires: tests shorten it.
    pub fn with_signin_ttl(mut self, ttl: Duration) -> Pool {
        self.signin_ttl = ttl;
        self
    }

    /// Begins signing in to `name` (R-MCP-22): returns the page to open and
    /// whether the host itself listens for the browser's redirect
    /// (`loopback`, for a host on the person's own machine; else the person
    /// pastes the address to `sign_in_finish`). Writes nothing until it
    /// finishes; a sign-in already begun for `name` is dropped.
    pub async fn sign_in_start(self: &std::sync::Arc<Pool>, name: &str, loopback: bool) -> Result<(String, bool), crate::engine::EngineError> {
        use crate::engine::EngineError;
        let found = self.kept.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| s.name == name).map(|(s, _)| s.config.url.clone());
        let Some(url) = found else { return Err(EngineError::new("no_such_server", format!("this session has no MCP server {name:?}"))) };
        let (Some(url), Some(creds)) = (url, self.credentials.clone()) else { return Err(EngineError::new("not_remote", format!("{name} is not a remote server: there is nothing to sign in to"))) };
        let http = crate::http::client()?;
        let pending = crate::mcp_oauth::start(&http, name, &url).await?;
        let page = pending.authorize_url.clone();
        let wait = if loopback {
            let (pool, server) = (self.clone(), name.to_string());
            SignInWait::Loopback(Abort(tokio::spawn(async move {
                if pending.finish_loopback(&http, &creds).await.is_ok() {
                    pool.reconnect(&server).await;
                }
            })))
        } else {
            SignInWait::Paste(Box::new(pending))
        };
        self.signins.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), SignIn { at: std::time::Instant::now(), wait });
        Ok((page, loopback))
    }

    /// Finishes the sign-in `sign_in_start` began with the address the
    /// browser landed on (never logged or echoed). Refused when none is
    /// waiting for a paste, when it expired, or when its state is not this
    /// sign-in's; the attempt is spent either way.
    pub async fn sign_in_finish(self: &std::sync::Arc<Pool>, name: &str, landing: &str) -> Result<(), crate::engine::EngineError> {
        use crate::engine::EngineError;
        let taken = self.signins.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        let Some(SignIn { at, wait: SignInWait::Paste(pending) }) = taken else {
            return Err(EngineError::new("no_sign_in", format!("no sign-in to {name} is waiting for an address — start one first")));
        };
        if at.elapsed() > self.signin_ttl {
            return Err(EngineError::new("sign_in_expired", format!("the sign-in to {name} expired (ten minutes) — start again")));
        }
        let creds = self.credentials.clone().ok_or_else(|| EngineError::new("not_remote", format!("{name} has nowhere to keep a sign-in")))?;
        pending.finish_pasted(&crate::http::client()?, &creds, landing).await?;
        self.reconnect(name).await;
        Ok(())
    }

    /// Each server's status and tools, in configuration order.
    pub fn status(&self) -> Vec<ServerStatus> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.iter().map(|(s, slot)| status_of(&s.name, &s.source, slot.current(None).get())).collect()
    }

    /// Starts `name` again at once, ending its connection first; it connects
    /// in the background (poll `status`). False when the pool has no such
    /// server.
    pub async fn reconnect(&self, name: &str) -> bool {
        let found = self.kept.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| s.name == name).cloned();
        let Some((server, slot)) = found else { return false };
        let old = slot.reset();
        if let Some(conn) = old.get().and_then(|l| l.conn.as_ref()) {
            conn.close().await;
        }
        let fresh = slot.current(None);
        let creds = self.credentials.clone();
        tokio::spawn(async move {
            boot(&fresh, &server, creds.as_deref()).await;
        });
        true
    }

    /// Signs `name` out (R-MCP-12): its tokens are removed, its connection
    /// closed, and its status becomes needs sign-in. Removing under the
    /// credentials lock first means a refresh in flight finds the sign-in
    /// gone and writes nothing back. False when the pool has no such server.
    pub async fn sign_out(&self, name: &str) -> Result<bool, crate::engine::EngineError> {
        let found = self.kept.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| s.name == name).cloned();
        let Some((_, slot)) = found else { return Ok(false) };
        self.signins.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        if let Some(c) = &self.credentials {
            // Blocks on the credentials lock, which a refresh may hold across its request.
            let c = c.clone();
            let name = name.to_string();
            tokio::task::spawn_blocking(move || crate::mcp_oauth::remove(&c, &name)).await.map_err(|e| crate::engine::EngineError::new("credentials_unwritable", e.to_string()))??;
        }
        let old = slot.reset();
        if let Some(conn) = old.get().and_then(|l| l.conn.as_ref()) {
            conn.close().await;
        }
        let ended = Started::default();
        let _ = ended.set(Live { name: name.into(), conn: None, tools: Vec::new(), error: None, left_out: None, at: std::time::Instant::now(), signin: true });
        *slot.0.lock().unwrap_or_else(|e| e.into_inner()) = ended;
        Ok(true)
    }

    /// The turn's `Mcp` over `servers`: a server unchanged since the last
    /// turn keeps its connection, one changed (its configuration or where
    /// it starts) starts again on first use (a failed one waits for its retry),
    /// and every connection left over — a server removed, untrusted, or
    /// replaced — is closed before this returns.
    pub async fn adopt(&self, servers: Vec<Server>) -> Mcp {
        let mut old: Vec<Option<(Server, Cell)>> = std::mem::take(&mut *self.kept.lock().unwrap_or_else(|e| e.into_inner())).into_iter().map(Some).collect();
        let mut live = Vec::with_capacity(servers.len());
        for s in &servers {
            let same = old.iter().position(|o| o.as_ref().is_some_and(|(k, _)| k.name == s.name && k.config == s.config && k.cwd == s.cwd));
            live.push(same.and_then(|i| old[i].take()).map_or_else(Cell::default, |(_, cell)| cell));
        }
        *self.kept.lock().unwrap_or_else(|e| e.into_inner()) = servers.iter().cloned().zip(live.iter().cloned()).collect();
        close_all(old.into_iter().flatten().map(|(_, c)| c).collect()).await;
        Mcp { servers, live, retry: self.retry, credentials: self.credentials.clone() }
    }

    /// Closes every connection: processes stopped, HTTP sessions ended.
    pub async fn close(&self) {
        self.signins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let kept = std::mem::take(&mut *self.kept.lock().unwrap_or_else(|e| e.into_inner()));
        close_all(kept.into_iter().map(|(_, c)| c).collect()).await;
    }
}

async fn close_all(cells: Vec<Cell>) {
    let started: Vec<Started> = cells.iter().map(|c| c.current(None)).collect();
    let closes = started.iter().filter_map(|c| c.get()).filter_map(|l| l.conn.as_ref()).map(|c| -> crate::engine::BoxFuture<'_, ()> { Box::pin(c.close()) });
    crate::native::join_all(closes.collect()).await;
}

impl Live {
    /// Whether it cannot serve a call: it never started, or its process was
    /// cut off or exited.
    fn dead(&self) -> bool {
        self.conn.as_ref().is_none_or(Conn::dead)
    }

    /// Why it cannot.
    fn needs_sign_in(&self) -> bool {
        self.signin || self.conn.as_ref().is_some_and(Conn::signin)
    }

    fn reason(&self) -> String {
        self.error.clone().unwrap_or_else(|| "its process exited or the connection was lost".into())
    }
}

fn status_of(name: &str, source: &str, live: Option<&Live>) -> ServerStatus {
    let Some(l) = live else { return ServerStatus { name: name.into(), source: source.into(), status: Status::Connecting, tools: Vec::new() } };
    let status = if l.needs_sign_in() {
        Status::NeedsSignIn
    } else if l.dead() {
        Status::Failed(l.reason())
    } else {
        Status::Connected
    };
    ServerStatus { name: name.into(), source: source.into(), status, tools: l.tools.iter().map(|t| (t.name.clone(), t.description.lines().next().unwrap_or_default().to_string())).collect() }
}

impl std::fmt::Debug for Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mcp").field("servers", &self.servers).finish()
    }
}

/// Find tools on the connected MCP servers. Its schema, and `CallInput`'s,
/// ride on every model call of a session with MCP servers, so each word is
/// one the model needs: unknown fields are ignored rather than refused,
/// which spares the schema its `additionalProperties`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchInput {
    /// What the tool does, or `server:tool`.
    pub query: String,
}

/// Call an MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallInput {
    /// Its `server:tool` name.
    pub tool: String,
    /// Its arguments.
    #[serde(default)]
    pub input: Option<serde_json::Map<String, Value>>,
}

/// A call's `server:tool`, split, or why it is not one.
pub fn target(input: &Value) -> Result<(String, String), (String, bool)> {
    let i = CallInput::deserialize(input).map_err(|e| (format!("invalid input for {CALL}: {e}"), true))?;
    match i.tool.split_once(':') {
        // Checked before the call is judged: an approval prompt names it.
        Some((s, t)) if good_name(s) && good_tool_name(t) => Ok((s.to_string(), t.to_string())),
        _ => Err((format!("{CALL} takes a tool as `server:tool`, as {SEARCH} returns it — not {:?}", i.tool), true)),
    }
}

/// A call's arguments.
pub fn arguments(input: &Value) -> Value {
    input.get("input").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!({}))
}

impl Default for Mcp {
    fn default() -> Mcp {
        Mcp::new(Vec::new())
    }
}

impl Mcp {
    pub fn new(servers: Vec<Server>) -> Mcp {
        let live = servers.iter().map(|_| Cell::default()).collect();
        Mcp { servers, live, retry: RETRY_AFTER, credentials: None }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// The two meta-tools: all the context MCP costs until it is used.
    /// They name the servers `reaches` keeps — a subagent's, when its
    /// definition names some — and are none when it keeps none.
    pub fn definitions(&self, reaches: &dyn Fn(&str) -> bool) -> Vec<crate::protocol::ToolDefinition> {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).filter(|n| reaches(n)).collect();
        if names.is_empty() {
            return Vec::new();
        }
        vec![
            crate::protocol::ToolDefinition {
                name: SEARCH.into(),
                description: format!("Find tools on the MCP servers ({}): returns their names and input schemas.", names.join(", ")),
                input_schema: crate::tools::input_schema::<SearchInput>(),
                grammar: None,
            },
            crate::protocol::ToolDefinition { name: CALL.into(), description: format!("Call a tool {SEARCH} found."), input_schema: crate::tools::input_schema::<CallInput>(), grammar: None },
        ]
    }

    /// Starts, in the background, each server `denied` does not cover whole
    /// that has not started: the turn goes on, and a call to one still
    /// connecting waits for it.
    pub fn warm(&self, denied: Denied<'_>) {
        for (s, slot) in self.servers.iter().zip(&self.live) {
            let cell = slot.current(None);
            if cell.get().is_some() || denied(&s.name, None) {
                continue;
            }
            let (s, creds) = (s.clone(), self.credentials.clone());
            tokio::spawn(async move {
                boot(&cell, &s, creds.as_deref()).await;
            });
        }
    }

    /// How long a failed `s` waits for another start: not at all when it
    /// needed a sign-in and one is stored now.
    fn retry_after(&self, s: &Server, slot: &Cell) -> Duration {
        let signin = slot.current(None).get().is_some_and(Live::needs_sign_in);
        let stored = || self.credentials.as_deref().zip(s.config.url.as_deref()).is_some_and(|(c, u)| crate::mcp_oauth::load(c, &s.name, u).ok().flatten().is_some_and(|t| t.refresh_token.is_some() || t.fresh(krowk_store::now_ms())));
        if signin && stored() { Duration::ZERO } else { self.retry }
    }

    /// Starts every server `denied` does not cover whole — a deny rule's,
    /// or one a subagent's allowlist leaves out — that is not running yet,
    /// at once, and waits for each to list its tools: those servers, in
    /// order. A failed one is started again if its last start was `retry`
    /// ago. An interrupt stops the wait, and the next call starts them
    /// again.
    async fn started(&self, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> Result<Vec<Started>, String> {
        let start_all = async {
            let starts = self.servers.iter().zip(&self.live).filter(|(s, _)| !denied(&s.name, None)).map(|(s, slot)| -> crate::engine::BoxFuture<'_, Started> {
                let cell = slot.current(Some(self.retry_after(s, slot)));
                Box::pin(async move {
                    boot(&cell, s, self.credentials.as_deref()).await;
                    cell
                })
            });
            crate::native::join_all(starts.collect()).await
        };
        let mut cancel = cancel.clone();
        tokio::select! {
            live = start_all => Ok(live),
            _ = cancel.wait_for(|c| *c) => Err("interrupted while the MCP servers started".into()),
        }
    }

    /// Runs `mcp_search`: the tools that match, best first, with their
    /// schemas, leaving out what a deny rule covers and what a subagent's
    /// allowlist does not name.
    pub async fn search(&self, input: &Value, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> (String, bool) {
        let query = match SearchInput::deserialize(input) {
            Ok(i) => i.query.trim().to_lowercase(),
            Err(e) => return (format!("invalid input for {SEARCH}: {e}"), true),
        };
        let live = match self.started(denied, cancel).await {
            Ok(l) => l,
            Err(e) => return (e, true),
        };
        let live: Vec<&Live> = live.iter().filter_map(|c| c.get()).collect();
        let words: Vec<&str> = query.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
        let visible = || live.iter().flat_map(|l| l.tools.iter().map(move |t| (*l, t))).filter(|(l, t)| !denied(&l.name, Some(&t.name)));
        let mut hits: Vec<(usize, &Live, &Tool)> = Vec::new();
        for (l, t) in visible() {
            let full = format!("{}:{}", l.name, t.name).to_lowercase();
            let hay = format!("{full} {}", t.description.to_lowercase());
            let score = if full == query { usize::MAX } else { words.iter().filter(|w| hay.contains(**w)).count() };
            if score > 0 {
                hits.push((score, l, t));
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        // Each schema is at most MAX_SCHEMA (the list refused bigger ones),
        // and the matches stop before the whole would pass MAX_SEARCH.
        let mut found: Vec<Value> = Vec::new();
        let mut size = 0;
        for (_, l, t) in hits.iter().take(MATCHES) {
            let hit = json!({"tool": format!("{}:{}", l.name, t.name), "description": crate::http::clip(&t.description, 1000), "input_schema": t.schema});
            size += hit.to_string().len();
            if size > MAX_SEARCH && !found.is_empty() {
                break;
            }
            found.push(hit);
        }
        let mut out = if found.is_empty() {
            let mut names = String::new();
            for (l, t) in visible() {
                let name = format!("{}:{}", l.name, t.name);
                if names.len() + name.len() > MAX_NAMES {
                    names.push_str(", … (search by what the tool does to find the rest)");
                    break;
                }
                names.push_str(if names.is_empty() { "" } else { ", " });
                names.push_str(&name);
            }
            format!("No MCP tool matches {query:?}. The tools are: {}", if names.is_empty() { "none" } else { &names })
        } else {
            serde_json::to_string_pretty(&found).expect("matches serialize")
        };
        for l in &live {
            if let Some(e) = &l.error {
                out.push_str(&format!("\n(the MCP server {} is not running: {e})", l.name));
            }
            if let Some(why) = &l.left_out {
                out.push_str(&format!("\n(the MCP server {}: {why})", l.name));
            }
        }
        (out, false)
    }

    /// Runs `mcp_call`, which the evaluator has allowed. A tool `denied`
    /// covers is refused here too, before any server starts: the native
    /// loop refuses a subagent's call outside its allowlist before it is
    /// judged, and this holds it for any other caller.
    pub async fn call(&self, input: &Value, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> (String, bool) {
        let (server, tool) = match target(input) {
            Ok(t) => t,
            Err(e) => return e,
        };
        if denied(&server, Some(&tool)) {
            return (format!("{server}:{tool} was not run: this turn may not reach it"), true);
        }
        let live = match self.started(denied, cancel).await {
            Ok(l) => l,
            Err(e) => return (e, true),
        };
        let live: Vec<&Live> = live.iter().filter_map(|c| c.get()).collect();
        let Some(l) = live.iter().find(|l| l.name == server) else {
            let names: Vec<&str> = live.iter().map(|l| l.name.as_str()).collect();
            return (format!("there is no MCP server named {server:?} — the servers are {}", names.join(", ")), true);
        };
        let Some(conn) = l.conn.as_ref().filter(|c| !c.dead()) else {
            if l.needs_sign_in() {
                return (needs_sign_in(&server), true);
            }
            let wait = self.retry.saturating_sub(l.at.elapsed()).as_secs();
            return (format!("the MCP server {server} is not running: {} (it is tried again by a call in {wait}s)", l.reason()), true);
        };
        if !l.tools.iter().any(|t| t.name == tool) {
            return (format!("the MCP server {server} has no tool named {tool:?} — find one with {SEARCH}"), true);
        }
        let mut cancel = cancel.clone();
        let req = conn.request("tools/call", json!({"name": tool, "arguments": arguments(input)}));
        // `None` when the call timed out or was interrupted: cut off
        // mid-message, the connection cannot be used again, so its process
        // group goes now rather than at the turn's end.
        let answer = tokio::select! {
            r = tokio::time::timeout(CALL_TIMEOUT, req) => r.ok(),
            _ = cancel.wait_for(|c| *c) => None,
        };
        let Some(answer) = answer else {
            conn.kill().await;
            let why = if *cancel.borrow() { "was interrupted".to_string() } else { format!("did not answer within {}s, so its server was stopped", CALL_TIMEOUT.as_secs()) };
            return (format!("{server}:{tool} {why}"), true);
        };
        match answer {
            Ok(r) => render(&r),
            Err(_) if conn.signin() => (needs_sign_in(&server), true),
            Err(e) => (format!("the MCP tool {server}:{tool} failed: {e}"), true),
        }
    }
}

/// A `tools/call` result as the model reads it: its text, and what else
/// it held named.
fn render(r: &Value) -> (String, bool) {
    let mut out = String::new();
    for c in r.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
        if !out.is_empty() {
            out.push('\n');
        }
        match c.get("type").and_then(|t| t.as_str()) {
            Some("text") => out.push_str(c.get("text").and_then(|t| t.as_str()).unwrap_or_default()),
            Some("resource") => out.push_str(c.pointer("/resource/text").and_then(|t| t.as_str()).unwrap_or("[a resource]")),
            Some(other) => out.push_str(&format!("[{other} content omitted]")),
            None => {}
        }
    }
    if out.is_empty()
        && let Some(s) = r.get("structuredContent")
    {
        out = s.to_string();
    }
    if out.len() > MAX_OUTPUT {
        out = crate::http::clip(&out, MAX_OUTPUT);
    }
    (out, r.get("isError").and_then(|e| e.as_bool()).unwrap_or(false))
}

/// Connects, initializes and lists the tools, following `nextCursor`.
async fn start(s: &Server, credentials: Option<&Path>) -> Result<(Conn, (Vec<Tool>, Option<String>)), String> {
    let conn = Conn::open(&s.config, &s.cwd, &s.name, credentials)?;
    conn.request("initialize", json!({"protocolVersion": MCP_VERSION, "capabilities": {}, "clientInfo": {"name": "krowk", "version": env!("CARGO_PKG_VERSION")}})).await?;
    conn.notify("notifications/initialized").await?;
    let (mut tools, mut listed, mut big, mut unnamed) = (Vec::new(), 0usize, 0usize, 0usize);
    let mut left_out = None;
    let mut cursor: Option<Value> = None;
    for page in 1..=MAX_PAGES {
        let params = cursor.take().map_or_else(|| json!({}), |c| json!({"cursor": c}));
        let r = conn.request("tools/list", params).await?;
        let got = r.get("tools").and_then(|t| t.as_array()).map_or(&[][..], |t| t.as_slice());
        for t in got {
            let Some(name) = t.get("name").and_then(|n| n.as_str()).filter(|n| good_tool_name(n)) else {
                unnamed += 1;
                continue;
            };
            let schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"}));
            let description: String = t.get("description").and_then(|d| d.as_str()).unwrap_or_default().into();
            let schema_len = schema.to_string().len();
            if schema_len > MAX_SCHEMA {
                big += 1;
                continue;
            }
            listed += name.len() + description.len() + schema_len;
            if listed > MAX_LISTED {
                left_out = Some(format!("its tools add up to more than {} MiB, so the rest were left out", MAX_LISTED >> 20));
                break;
            }
            tools.push(Tool { name: name.into(), description, schema });
        }
        // A page with no tools that still names a next one is a server
        // paging forever.
        match r.get("nextCursor") {
            Some(c) if !c.is_null() && !got.is_empty() && left_out.is_none() => cursor = Some(c.clone()),
            _ => break,
        }
        if page == MAX_PAGES {
            left_out = Some(format!("it listed more than {MAX_PAGES} pages of tools, so the rest were left out"));
        }
    }
    let notes = [(big > 0).then(|| format!("{big} tool(s) with an input schema over {} KiB were left out", MAX_SCHEMA >> 10)), (unnamed > 0).then(|| format!("{unnamed} tool(s) named with a control character, whitespace or `*`, or with no name, were left out"))];
    for note in notes.into_iter().flatten() {
        left_out = Some(left_out.map_or(note.clone(), |l| format!("{l}; {note}")));
    }
    Ok((conn, (tools, left_out)))
}

/// A tool name as a server lists it, or the model gives it: printable, so
/// the approval prompt that names it shows what it is, and holding no `*`,
/// which a rule and an allowlist read as every tool, so a tool named so
/// could never be told from all of its server's.
pub(crate) fn good_tool_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && !n.chars().any(|c| c.is_control() || c.is_whitespace() || c == '*')
}

/// A connection to one server.
enum Conn {
    /// Line-delimited JSON-RPC on the child's stdin and stdout, one request
    /// at a time. A request cut off by an interrupt may leave half a line
    /// either way, so the connection is not used again (`cut`).
    /// `junked` when it was stopped for writing what is not JSON-RPC.
    Stdio { io: Box<tokio::sync::Mutex<StdioIo>>, pid: Option<u32>, reaped: AtomicBool, next: AtomicU64, cut: AtomicBool, junked: AtomicBool },
    /// Streamable HTTP: each message a POST, answered with JSON or an SSE
    /// stream, the session id carried from `initialize` on.
    Http { client: reqwest::Client, url: String, headers: BTreeMap<String, String>, session: std::sync::Mutex<Option<String>>, next: AtomicU64, lost: AtomicBool, auth: Option<Auth> },
}

/// A remote server's stored sign-in, used on each request (R-MCP-10).
struct Auth {
    name: String,
    url: String,
    credentials: PathBuf,
    /// The last request found the server wanting a sign-in.
    signin: AtomicBool,
    /// A token was sent before: its disappearing means a logout.
    had: AtomicBool,
}

impl Auth {
    async fn token(&self, client: &reqwest::Client, refused: Option<&str>) -> Result<Option<String>, String> {
        match crate::mcp_oauth::bearer(client, &self.credentials, &self.name, &self.url, refused).await {
            Ok(crate::mcp_oauth::Bearer::Token(t)) => Ok(Some(t)),
            Ok(crate::mcp_oauth::Bearer::SignIn) => Ok(None),
            Err(e) => Err(e.message),
        }
    }
}

/// What a call to a server that needs sign-in returns (R-MCP-11).
fn needs_sign_in(name: &str) -> String {
    format!("{name} needs sign-in: run `krowk mcp login {name}` or open /mcp")
}

struct StdioIo {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    /// Dropping it kills the server (`kill_on_drop`), and on unix its whole
    /// process group: `npx` and `uvx` start the real server as a child.
    child: tokio::process::Child,
}

impl StdioIo {
    fn kill(&mut self) {
        kill_group(self.child.id());
        let _ = self.child.start_kill();
    }
}

/// Stops the group a stdio server leads (`process_group(0)` below).
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pid;
}

impl Drop for StdioIo {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Conn {
    fn open(c: &ServerConfig, cwd: &Path, name: &str, credentials: Option<&Path>) -> Result<Conn, String> {
        let next = AtomicU64::new(1);
        match (c.kind.as_deref(), &c.command, &c.url) {
            (Some("http" | "streamable-http") | None, None, Some(url)) => {
                let client = crate::http::client().map_err(|e| e.message)?;
                // A header of the person's own named Authorization wins.
                let auth = credentials.filter(|_| !c.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"))).map(|p| Auth { name: name.into(), url: url.clone(), credentials: p.into(), signin: AtomicBool::new(false), had: AtomicBool::new(false) });
                Ok(Conn::Http { client, url: url.clone(), headers: c.headers.clone(), session: std::sync::Mutex::new(None), next, lost: AtomicBool::new(false), auth })
            }
            (Some("sse"), ..) => Err("the SSE transport is not supported; use a stdio or streamable-HTTP server".into()),
            (Some("stdio") | None, Some(cmd), _) => {
                let mut command = tokio::process::Command::new(program(cmd, cwd)?);
                for (k, _) in std::env::vars_os() {
                    let Some(k) = k.to_str() else { continue };
                    if (SCRUBBED.contains(&k) || k.starts_with("KROWK_")) && !c.env.contains_key(k) {
                        command.env_remove(k);
                    }
                }
                #[cfg(unix)]
                command.process_group(0);
                let mut child = command
                    .args(&c.args)
                    .envs(&c.env)
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    // Its logging would land on the terminal the TUI draws.
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("{cmd} could not be started: {e}"))?;
                let pid = child.id();
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
                Ok(Conn::Stdio { io: Box::new(tokio::sync::Mutex::new(StdioIo { stdin, stdout, child })), pid, reaped: AtomicBool::new(false), next, cut: AtomicBool::new(false), junked: AtomicBool::new(false) })
            }
            (kind, ..) => Err(format!("it names neither a command nor a url for its transport ({})", kind.unwrap_or("stdio"))),
        }
    }

    /// Whether a stdio server was cut off, stopped or has exited: it is not
    /// used again, and the next turn starts it afresh.
    /// An HTTP server that no longer knows its session (404) counts too.
    fn dead(&self) -> bool {
        match self {
            Conn::Stdio { cut, io, reaped, .. } => {
                // A call in flight holds the lock, and has `cut` set while it
                // waits: that is a server at work, not a dead one.
                let Ok(mut io) = io.try_lock() else { return false };
                let exited = io.child.try_wait().map_or(true, |s| s.is_some());
                // An exited server is reaped: its pid is no longer ours to signal.
                reaped.fetch_or(exited, Ordering::AcqRel);
                cut.load(Ordering::Acquire) || exited
            }
            Conn::Http { lost, .. } => lost.load(Ordering::Acquire),
        }
    }

    /// Whether the last request found the server wanting a sign-in.
    fn signin(&self) -> bool {
        matches!(self, Conn::Http { auth: Some(a), .. } if a.signin.load(Ordering::Acquire))
    }

    /// Lets go of the connection when its session does: a stdio server's
    /// group is stopped without waiting on a call that holds it, and an
    /// HTTP session with an id is ended with DELETE.
    async fn close(&self) {
        match self {
            Conn::Stdio { pid, cut, reaped, .. } => {
                cut.store(true, Ordering::Release);
                if !reaped.load(Ordering::Acquire) {
                    kill_group(*pid);
                }
            }
            Conn::Http { client, url, headers, session, auth, .. } => {
                let Some(id) = session.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
                let mut req = client.delete(url).header("mcp-session-id", id).header("mcp-protocol-version", MCP_VERSION);
                if let Some(a) = auth
                    && let Ok(Some(t)) = a.token(client, None).await
                {
                    req = req.bearer_auth(t);
                }
                for (k, v) in headers {
                    req = req.header(k, v);
                }
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, req.send()).await;
            }
        }
    }

    /// Stops a stdio server and its process group; it is not used again.
    async fn kill(&self) {
        if let Conn::Stdio { io, cut, .. } = self {
            cut.store(true, Ordering::Release);
            io.lock().await.kill();
        }
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match self {
            Conn::Stdio { io, .. } => write_line(&mut io.lock().await.stdin, &msg).await,
            Conn::Http { .. } => self.post(&msg, None).await.map(|_| ()),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = match self {
            Conn::Stdio { next, .. } | Conn::Http { next, .. } => next.fetch_add(1, Ordering::Relaxed),
        };
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let answer = match self {
            Conn::Stdio { io, cut, junked, .. } => {
                let mut io = io.lock().await;
                if cut.swap(true, Ordering::AcqRel) {
                    if junked.load(Ordering::Acquire) {
                        return Err("it was stopped earlier this turn for writing lines that are not JSON-RPC, so it is not used again this turn".into());
                    }
                    return Err("an earlier call to this server was cut off mid-message (an interrupt, or a pipe that broke), so it is not used again this turn".into());
                }
                write_line(&mut io.stdin, &msg).await?;
                let (mut junk, mut junk_bytes) = (0usize, 0usize);
                let answer = loop {
                    let mut line = String::new();
                    let read = (&mut io.stdout).take(MAX_MESSAGE).read_line(&mut line).await.map_err(|e| e.to_string())?;
                    if read == 0 {
                        return Err("the server exited".into());
                    }
                    if !line.ends_with('\n') && read as u64 == MAX_MESSAGE {
                        return Err(format!("the server sent a message over {} MiB", MAX_MESSAGE >> 20));
                    }
                    // A line that is no JSON-RPC message — a banner, a log
                    // line on the wrong stream — is passed over, but only so
                    // many: a server writing nothing else would otherwise
                    // hold the call to CALL_TIMEOUT. It is stopped, as one cut
                    // off is (`cut` stays set).
                    // An object with neither `jsonrpc` nor `id` — a JSON
                    // log line — is no message either.
                    let Some(v) = serde_json::from_str::<Value>(&line).ok().filter(|v| v.get("jsonrpc").is_some() || v.get("id").is_some()) else {
                        junk += 1;
                        junk_bytes += read;
                        if junk > MAX_JUNK_LINES || junk_bytes > MAX_JUNK_BYTES {
                            io.kill();
                            junked.store(true, Ordering::Release);
                            return Err(format!("it wrote {junk} lines that are not JSON-RPC ({} KiB) while krowk waited for its answer, so it was stopped", junk_bytes >> 10));
                        }
                        continue;
                    };
                    if is_answer(&v, id) {
                        break v;
                    }
                    // A request of the server's own — ping, roots, sampling:
                    // ping is answered, the rest refused, so it never waits.
                    if let (Some(rid), Some(m)) = (v.get("id"), v.get("method").and_then(|m| m.as_str())) {
                        let reply = if m == "ping" {
                            json!({"jsonrpc": "2.0", "id": rid, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "krowk does not offer this"}})
                        };
                        write_line(&mut io.stdin, &reply).await?;
                    }
                };
                cut.store(false, Ordering::Release);
                answer
            }
            Conn::Http { .. } => self.post(&msg, Some(id)).await?.ok_or("the server sent no answer")?,
        };
        match (answer.get("result"), answer.get("error")) {
            (_, Some(e)) => Err(e.get("message").and_then(|m| m.as_str()).map_or_else(|| e.to_string(), String::from)),
            (Some(r), _) => Ok(r.clone()),
            _ => Err("the answer held neither a result nor an error".into()),
        }
    }

    /// One POST; the answer to `id`, from a JSON body or from an SSE stream
    /// read only as far as the answer, since a server may keep it open.
    async fn post(&self, msg: &Value, id: Option<u64>) -> Result<Option<Value>, String> {
        let Conn::Http { client, auth, lost, .. } = self else { unreachable!("post is HTTP's") };
        let Some(a) = auth else { return self.post_once(msg, id, None).await };
        let token = a.token(client, None).await?;
        if token.is_none() && a.had.swap(false, Ordering::AcqRel) {
            // Its sign-in was removed while this connection held it: the
            // connection is let go, and the next call starts afresh.
            a.signin.store(true, Ordering::Release);
            lost.store(true, Ordering::Release);
            return Err("its sign-in was removed".into());
        }
        let mut r = self.post_once(msg, id, token.as_deref()).await;
        // A 401 on a token: one refresh, one retry.
        if let (Some(t), Err(e)) = (&token, &r)
            && e.starts_with("HTTP 401")
        {
            r = match a.token(client, Some(t)).await? {
                Some(t2) => self.post_once(msg, id, Some(&t2)).await,
                None => r,
            };
        }
        let wants = r.as_ref().err().is_some_and(|e| e.starts_with("HTTP 401"));
        a.signin.store(wants, Ordering::Release);
        if token.is_some() && !wants {
            a.had.store(true, Ordering::Release);
        }
        r
    }

    async fn post_once(&self, msg: &Value, id: Option<u64>, bearer: Option<&str>) -> Result<Option<Value>, String> {
        let Conn::Http { client, url, headers, session, lost, .. } = self else { unreachable!("post is HTTP's") };
        let mut req = client.post(url).header("content-type", "application/json").header("accept", "application/json, text/event-stream").header("mcp-protocol-version", MCP_VERSION);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let sent = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(s) = &sent {
            req = req.header("mcp-session-id", s);
        }
        let mut resp = req.body(msg.to_string()).send().await.map_err(|e| e.to_string())?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.to_string());
        }
        let status = resp.status();
        // 404 on a session id: the server forgot it (it restarted); the
        // next turn starts the server afresh.
        if sent.is_some() && status == reqwest::StatusCode::NOT_FOUND {
            lost.store(true, Ordering::Release);
        }
        let sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
        // An SSE stream is parsed as it arrives and only its unfinished
        // line is kept, so the cap is per event, however long the stream;
        // any other body is capped whole.
        let mut body: Vec<u8> = Vec::new();
        let streaming = sse && status.is_success();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if streaming && let Some(id) = id {
                let whole = body.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                let lines: Vec<u8> = body.drain(..whole).collect();
                if let Some(v) = sse_answer(&String::from_utf8_lossy(&lines), id) {
                    return Ok(Some(v));
                }
            }
            if body.len() as u64 > MAX_MESSAGE {
                return Err(format!("the server sent a message over {} MiB", MAX_MESSAGE >> 20));
            }
        }
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", crate::http::clip(&body, 300)));
        }
        let Some(id) = id else { return Ok(None) };
        if sse {
            return Ok(sse_answer(&body, id));
        }
        serde_json::from_str::<Value>(&body).map(|v| Some(v).filter(|v| is_answer(v, id))).map_err(|e| format!("the answer is not JSON: {e}"))
    }
}

/// The program a stdio server's `command` names, resolved here rather than
/// by the OS: a relative path against the server's own working directory
/// (never krowk's, which may be a repository nobody trusted), and a bare
/// name through the absolute directories of `PATH` only, since a relative
/// entry there (`.`) would also resolve against krowk's.
fn program(cmd: &str, cwd: &Path) -> Result<PathBuf, String> {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if p.components().count() > 1 || cmd.starts_with('.') {
        return Ok(cwd.join(p));
    }
    #[cfg(unix)]
    if let Some(found) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).filter(|d| d.is_absolute()).map(|d| d.join(cmd)).find(|f| {
            use std::os::unix::fs::PermissionsExt;
            f.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }) {
        return Ok(found);
    }
    #[cfg(unix)]
    return Err(format!("{cmd} is not on the PATH (only its absolute directories are searched)"));
    #[cfg(not(unix))]
    Ok(p.to_path_buf())
}

/// The answer to `id` among an SSE stream's `data:` lines.
fn sse_answer(lines: &str, id: u64) -> Option<Value> {
    lines.lines().filter_map(|l| l.strip_prefix("data:")).filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok()).find(|v| is_answer(v, id))
}

/// Whether a message is the answer to request `id`, not a request of the
/// server's own that happens to share the number.
fn is_answer(v: &Value, id: u64) -> bool {
    v.get("id").and_then(|i| i.as_u64()) == Some(id) && v.get("method").is_none()
}

async fn write_line(w: &mut tokio::process::ChildStdin, msg: &Value) -> Result<(), String> {
    let mut line = msg.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await.map_err(|e| format!("the server stopped reading: {e}"))?;
    w.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_tool_3_variables_expand_and_server_names_read_one_way_in_a_rule() {
        let get = |k: &str| (k == "TOKEN").then(|| "t0k".to_string());
        assert_eq!(expand_vars("Bearer ${TOKEN}", &get), "Bearer t0k");
        assert_eq!(expand_vars("${NOPE:-fallback}/x", &get), "fallback/x");
        assert_eq!(expand_vars("${NOPE}", &get), "");
        assert_eq!(expand_vars("a ${unclosed", &get), "a ${unclosed");
        assert!(good_name("github") && good_name("my-server_2"));
        for bad in ["a__b", "a:b", "", "s_", "_s", "a b"] {
            assert!(!good_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn r_tool_3_a_mcp_json_swapped_after_the_trust_check_is_not_loaded() {
        let base = std::env::temp_dir().join(format!("krowk-mcp-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("krowk")).unwrap();
        let repo = base.join("repo").canonicalize().unwrap();
        std::fs::write(repo.join(".mcp.json"), json!({"mcpServers": {"good": {"command": "good"}}}).to_string()).unwrap();
        let store = crate::trust::Store::new(Some(base.join("krowk").join(crate::trust::FILE)), None);
        store.trust(&repo).unwrap();
        // The check passed on the file the person was shown; then the file
        // is swapped before the servers are read.
        let checked = store.trusts(&repo);
        assert!(checked);
        std::fs::write(repo.join(".mcp.json"), json!({"mcpServers": {"evil": {"command": "evil"}}}).to_string()).unwrap();
        let cfg = crate::permissions::Config { krowk_dir: Some(base.join("krowk")), trusted: Some(std::sync::Arc::new(move |_: &Path| checked)), ..Default::default() };
        let names: Vec<String> = discover(&cfg, &repo).0.into_iter().map(|s| s.name).collect();
        assert!(names.is_empty(), "the swapped-in servers were loaded: {names:?}");
        // The file as trusted loads.
        std::fs::write(repo.join(".mcp.json"), json!({"mcpServers": {"good": {"command": "good"}}}).to_string()).unwrap();
        assert_eq!(discover(&cfg, &repo).0.into_iter().map(|s| s.name).collect::<Vec<_>>(), ["good"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_tool_3_a_call_naming_a_control_character_is_refused_before_it_is_judged() {
        assert_eq!(target(&json!({"tool": "fake:echo"})).unwrap(), ("fake".into(), "echo".into()));
        for bad in ["fake:echo\u{1b}[2J", "fake:a\nb", "fa ke:echo", "fake:", ":echo", "s_:x", "fake:*", "fake:get_*"] {
            assert!(target(&json!({"tool": bad})).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn r_tool_3_an_untrusted_repository_lists_no_project_server_and_starts_the_persons_in_their_home() {
        let base = std::env::temp_dir().join(format!("krowk-mcp-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        let repo = base.join("repo").canonicalize().unwrap();
        std::fs::write(repo.join(".mcp.json"), json!({"mcpServers": {"repo": {"command": "x"}, "github": {"command": "evil"}}}).to_string()).unwrap();
        let cfg = |trusted: bool| crate::permissions::Config {
            home: Some(base.join("home")),
            user: Some(json!({"mcpServers": {"github": {"command": "github-mcp-server"}}})),
            trusted: Some(std::sync::Arc::new(move |_: &Path| trusted)),
            ..Default::default()
        };
        let (untrusted, _) = discover(&cfg(false), &repo);
        assert_eq!(untrusted.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["github"]);
        assert_eq!(untrusted[0].cwd, base.join("home"), "not in the repository nobody trusted");
        let (trusted, _) = discover(&cfg(true), &repo);
        let github = trusted.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(github.config.command.as_deref(), Some("github-mcp-server"), "the person's own server outranks a repository's of the same name");
        assert!(trusted.iter().any(|s| s.name == "repo" && s.cwd == repo));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repository, trusted, with a `.mcp.json` naming `a`, `b` and `c`,
    /// started in its `app/` directory; Claude's files under `base/claude`.
    fn approval_repo(name: &str) -> (PathBuf, PathBuf, crate::permissions::Config) {
        let base = std::env::temp_dir().join(format!("krowk-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/app/.claude")).unwrap();
        std::fs::create_dir_all(base.join("repo/.claude")).unwrap();
        std::fs::create_dir_all(base.join("claude")).unwrap();
        let base = base.canonicalize().unwrap();
        std::fs::write(base.join("repo/.mcp.json"), json!({"mcpServers": {"a": {"command": "a"}, "b": {"command": "b"}, "c": {"command": "c"}}}).to_string()).unwrap();
        let cfg = crate::permissions::Config { home: Some(base.join("home")), claude_dir: Some(base.join("claude")), trusted: Some(std::sync::Arc::new(|_: &Path| true)), ..Default::default() };
        (base.clone(), base.join("repo/app"), cfg)
    }

    #[test]
    fn r_tool_3_claude_codes_enabled_and_disabled_mcpjson_servers_choose_among_a_trusted_projects() {
        let (base, app, cfg) = approval_repo("approval");
        let names = || discover(&cfg, &app).0.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(), ["a", "b", "c"], "trusted and named by no list: all of them, as the trust question covered");
        // Claude Code's own entry for every project it has opened: empty
        // lists, which turn nothing off.
        std::fs::write(base.join("claude/.claude.json"), json!({"projects": {app.display().to_string(): {"enabledMcpjsonServers": [], "disabledMcpjsonServers": []}}}).to_string()).unwrap();
        assert_eq!(names(), ["a", "b", "c"], "an empty enabled list is no list");
        // The person's dialog answers, in ~/.claude.json under the directory
        // Claude Code was started in.
        let claude_json = |v: Value| std::fs::write(base.join("claude/.claude.json"), json!({"projects": {app.display().to_string(): v}}).to_string()).unwrap();
        claude_json(json!({"enabledMcpjsonServers": ["a", "b"]}));
        assert_eq!(names(), ["a", "b"], "once an enabled list is set, only what it names");
        // A repository's own settings, down to the working directory, add
        // to the lists; disabled wins over enabled.
        std::fs::write(base.join("repo/.claude/settings.json"), json!({"disabledMcpjsonServers": ["b"]}).to_string()).unwrap();
        assert_eq!(names(), ["a"]);
        std::fs::write(base.join("repo/app/.claude/settings.local.json"), json!({"enableAllProjectMcpServers": true}).to_string()).unwrap();
        assert_eq!(names(), ["a", "c"], "enableAllProjectMcpServers overrides the enabled list, never the disabled one");
        // In Claude Code's user settings too.
        std::fs::remove_file(base.join("repo/app/.claude/settings.local.json")).unwrap();
        std::fs::write(base.join("claude/settings.json"), json!({"disabledMcpjsonServers": ["a"]}).to_string()).unwrap();
        assert!(names().is_empty(), "{:?}", names());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_tool_3_claude_json_projects_are_looked_up_by_the_launch_directory_then_the_repository() {
        let (base, app, cfg) = approval_repo("projects");
        std::fs::remove_file(base.join("repo/.mcp.json")).unwrap();
        let entry = |dir: &Path, cmd: &str| (dir.display().to_string(), json!({"mcpServers": {"local": {"command": cmd}}}));
        let write = |entries: Vec<(String, Value)>| std::fs::write(base.join("claude/.claude.json"), json!({"projects": entries.into_iter().collect::<serde_json::Map<_, _>>()}).to_string()).unwrap();
        let local = || discover(&cfg, &app).0.into_iter().find(|s| s.name == "local").and_then(|s| s.config.command);
        write(vec![entry(&base.join("repo"), "by-root")]);
        assert_eq!(local().as_deref(), Some("by-root"), "the repository's entry, when the launch directory has none");
        write(vec![entry(&base.join("repo"), "by-root"), entry(&app, "by-cwd")]);
        assert_eq!(local().as_deref(), Some("by-cwd"), "Claude Code keys a project by where it was started");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_tool_3_a_server_left_out_for_its_name_or_config_is_named_with_why() {
        let (base, app, cfg) = approval_repo("skipped");
        std::fs::write(base.join("repo/.mcp.json"), json!({"mcpServers": {"ok": {"command": "ok"}, "a__b": {"command": "x"}, "args": {"command": "x", "args": "not a list"}}}).to_string()).unwrap();
        let (servers, skipped) = discover(&cfg, &app);
        assert_eq!(servers.into_iter().map(|s| s.name).collect::<Vec<_>>(), ["ok"]);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().any(|w| w.contains("\"a__b\" in .mcp.json") && w.contains("no __")), "{skipped:?}");
        assert!(skipped.iter().any(|w| w.contains("args in .mcp.json was skipped: invalid type")), "{skipped:?}");
        // What serde quotes from a config — an env written as one string,
        // a token in it — is not repeated in the notice.
        std::fs::write(base.join("repo/.mcp.json"), json!({"mcpServers": {"s": {"command": "x", "env": "TOKEN=hunter2"}}}).to_string()).unwrap();
        let (_, skipped) = discover(&cfg, &app);
        assert!(skipped.len() == 1 && !skipped[0].contains("hunter2"), "{skipped:?}");
        std::fs::write(base.join("repo/.mcp.json"), json!({"mcpServers": {"s": {"command": "x", "env": "TOK\"EN=hunter2"}}}).to_string()).unwrap();
        let (_, skipped) = discover(&cfg, &app);
        assert!(skipped.len() == 1 && !skipped[0].contains("hunter2") && skipped[0].contains("expected"), "a quote inside the value: {skipped:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
