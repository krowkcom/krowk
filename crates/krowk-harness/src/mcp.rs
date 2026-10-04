//! MCP servers a native turn can use, with their tools deferred (R-TOOL-3).
//!
//! A person's MCP servers come from the places Claude Code keeps them, so a
//! setup made for it needs nothing new. Lowest first — a later source's
//! server replaces an earlier one of the same name:
//!
//! | source | file | read |
//! |---|---|---|
//! | project | `<repository>/.mcp.json`: `mcpServers` | only once the repository is trusted |
//! | Claude Code, user | Claude's `settings.json`, then `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json`): `mcpServers` | always |
//! | Claude Code, local | `~/.claude.json`: `projects["<repository>"].mcpServers` | always |
//! | krowk, user | `config.json` in krowk's home: `mcpServers` | always |
//!
//! The project is lowest, where Claude Code puts it above the user's: a
//! repository's `github` must not take the place of the person's, whose
//! allow rules name it. Each server is Claude Code's shape: `{command,
//! args, env}` for a stdio server, `{type: "http", url, headers}` for a
//! streamable-HTTP one, with `${VAR}` and `${VAR:-default}` expanded from
//! the environment.
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
/// How long one tool call may run.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Search returns at most this many tools, schemas and all.
const MATCHES: usize = 5;
/// A tool's output beyond this is cut, as bash's is.
const MAX_OUTPUT: usize = 30_000;
/// One message from a server may be at most this long: a server that
/// never ends a line cannot fill the memory.
const MAX_MESSAGE: u64 = 16 << 20;
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
fn good_name(n: &str) -> bool {
    !n.is_empty() && !n.contains("__") && !n.starts_with('_') && !n.ends_with('_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every server that applies in `cwd`, lowest source first, later names
/// replacing earlier ones. A project's `.mcp.json` counts only when its
/// repository is trusted.
pub fn discover(cfg: &crate::permissions::Config, cwd: &Path) -> Vec<Server> {
    let root = crate::trust::root(cwd);
    let trusted = cfg.trusted.as_ref().is_some_and(|t| t(&root));
    // Untrusted and with no home, there is nowhere safe to start a stdio
    // server: a shared temporary directory is anyone's to plant in.
    let runs_in = if trusted { Some(cwd.to_path_buf()) } else { cfg.home.clone() };
    let mut out: Vec<Server> = Vec::new();
    let mut add = |v: Option<&Value>, source: &str| {
        let Some(Value::Object(m)) = v else { return };
        for (name, c) in m {
            let Ok(config) = ServerConfig::deserialize(c) else { continue };
            if !good_name(name) {
                continue;
            }
            let Some(runs_in) = runs_in.clone().or_else(|| config.command.is_none().then(PathBuf::new)) else { continue };
            out.retain(|s| s.name != *name);
            out.push(Server { name: name.clone(), config: expand(config), source: source.to_string(), cwd: runs_in });
        }
    };
    let read = |p: &Path| std::fs::read(p).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    // Read once: the servers loaded are the ones whose digest is checked
    // here against what trust recorded, never a file swapped in after the
    // check (`trusted` read it too, but a moment earlier).
    if trusted && let Some(v) = read_project(&root) {
        let recorded = cfg.krowk_dir.as_ref().and_then(|d| crate::trust::Store::new(Some(d.join(crate::trust::FILE)), cfg.home.clone()).recorded_mcp(&root));
        // Not listed at all: trusted for this run (`--trust`, or a yes not
        // yet remembered), which covers what is there now.
        if recorded.is_none_or(|r| r == digest(&v)) {
            add(v.get("mcpServers"), ".mcp.json");
        }
    }
    if let Some(dir) = cfg.claude_home()
        && let Some(s) = read(&dir.join("settings.json"))
    {
        add(s.get("mcpServers"), "Claude Code's settings.json");
    }
    let claude_json = match &cfg.claude_dir {
        Some(d) => Some(d.join(".claude.json")),
        None => cfg.home.as_ref().map(|h| h.join(".claude.json")),
    };
    if let Some(v) = claude_json.as_deref().and_then(read) {
        add(v.get("mcpServers"), "~/.claude.json");
        add(v.get("projects").and_then(|p| p.get(root.display().to_string())).and_then(|p| p.get("mcpServers")), "~/.claude.json (this project)");
    }
    let user = match (&cfg.user_path, cfg.reread) {
        (Some(p), true) => read(p),
        _ => cfg.user.clone(),
    };
    add(user.as_ref().and_then(|u| u.get("mcpServers")), "krowk's config.json");
    out
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
    let p = root.join(".mcp.json");
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > MAX_MCP_JSON {
        return None;
    }
    serde_json::from_slice(&std::fs::read(&p).ok()?).ok()
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
}

/// Whether a deny rule covers `server`'s `tool` (`*`: the whole server).
pub type Denied<'a> = &'a (dyn Fn(&str, &str) -> bool + Sync);

/// The session's MCP servers: configured up front, started on first use.
/// Once started they are read without a lock, so one long call never holds
/// another, or a search, behind it.
#[derive(Default)]
pub struct Mcp {
    servers: Vec<Server>,
    live: tokio::sync::OnceCell<Vec<Live>>,
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

impl Mcp {
    pub fn new(servers: Vec<Server>) -> Mcp {
        Mcp { servers, live: tokio::sync::OnceCell::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// The two meta-tools: all the context MCP costs until it is used.
    pub fn definitions(&self) -> Vec<crate::protocol::ToolDefinition> {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
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

    /// Starts every server a deny rule does not cover whole, at once, and
    /// waits for each to list its tools; an interrupt stops the wait, and
    /// the next call starts them again.
    async fn started(&self, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> Result<&[Live], String> {
        let start_all = async {
            let starts = self.servers.iter().filter(|s| !denied(&s.name, "*")).map(|s| -> crate::engine::BoxFuture<'_, Live> {
                Box::pin(async move {
                    let (conn, (tools, left_out), error) = match tokio::time::timeout(START_TIMEOUT, start(s)).await {
                        Ok(Ok((conn, listed))) => (Some(conn), listed, None),
                        Ok(Err(e)) => (None, Default::default(), Some(e)),
                        Err(_) => (None, Default::default(), Some(format!("it did not start within {}s", START_TIMEOUT.as_secs()))),
                    };
                    Live { name: s.name.clone(), conn, tools, error, left_out }
                })
            });
            crate::native::join_all(starts.collect()).await
        };
        let mut cancel = cancel.clone();
        tokio::select! {
            live = self.live.get_or_init(|| start_all) => Ok(live.as_slice()),
            _ = cancel.wait_for(|c| *c) => Err("interrupted while the MCP servers started".into()),
        }
    }

    /// Runs `mcp_search`: the tools that match, best first, with their
    /// schemas, leaving out what a deny rule covers.
    pub async fn search(&self, input: &Value, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> (String, bool) {
        let query = match SearchInput::deserialize(input) {
            Ok(i) => i.query.trim().to_lowercase(),
            Err(e) => return (format!("invalid input for {SEARCH}: {e}"), true),
        };
        let live = match self.started(denied, cancel).await {
            Ok(l) => l,
            Err(e) => return (e, true),
        };
        let words: Vec<&str> = query.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
        let visible = || live.iter().flat_map(|l| l.tools.iter().map(move |t| (l, t))).filter(|(l, t)| !denied(&l.name, &t.name));
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
        for l in live {
            if let Some(e) = &l.error {
                out.push_str(&format!("\n(the MCP server {} is not running: {e})", l.name));
            }
            if let Some(why) = &l.left_out {
                out.push_str(&format!("\n(the MCP server {}: {why})", l.name));
            }
        }
        (out, false)
    }

    /// Runs `mcp_call`, which the evaluator has allowed.
    pub async fn call(&self, input: &Value, denied: Denied<'_>, cancel: &watch::Receiver<bool>) -> (String, bool) {
        let (server, tool) = match target(input) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let live = match self.started(denied, cancel).await {
            Ok(l) => l,
            Err(e) => return (e, true),
        };
        let Some(l) = live.iter().find(|l| l.name == server) else {
            let names: Vec<&str> = live.iter().map(|l| l.name.as_str()).collect();
            return (format!("there is no MCP server named {server:?} — the servers are {}", names.join(", ")), true);
        };
        let Some(conn) = &l.conn else { return (format!("the MCP server {server} is not running: {}", l.error.as_deref().unwrap_or_default()), true) };
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
async fn start(s: &Server) -> Result<(Conn, (Vec<Tool>, Option<String>)), String> {
    let conn = Conn::open(&s.config, &s.cwd)?;
    conn.request("initialize", json!({"protocolVersion": MCP_VERSION, "capabilities": {}, "clientInfo": {"name": "krowk", "version": env!("CARGO_PKG_VERSION")}})).await?;
    conn.notify("notifications/initialized").await?;
    let (mut tools, mut listed, mut big) = (Vec::new(), 0usize, 0usize);
    let mut left_out = None;
    let mut cursor: Option<Value> = None;
    for page in 1..=MAX_PAGES {
        let params = cursor.take().map_or_else(|| json!({}), |c| json!({"cursor": c}));
        let r = conn.request("tools/list", params).await?;
        let got = r.get("tools").and_then(|t| t.as_array()).map_or(&[][..], |t| t.as_slice());
        for t in got {
            let Some(name) = t.get("name").and_then(|n| n.as_str()).filter(|n| good_tool_name(n)) else { continue };
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
    if big > 0 {
        let note = format!("{big} tool(s) with an input schema over {} KiB were left out", MAX_SCHEMA >> 10);
        left_out = Some(left_out.map_or(note.clone(), |l| format!("{l}; {note}")));
    }
    Ok((conn, (tools, left_out)))
}

/// A tool name as a server lists it, or the model gives it: printable, so
/// the approval prompt that names it shows what it is.
fn good_tool_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && !n.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// A connection to one server.
enum Conn {
    /// Line-delimited JSON-RPC on the child's stdin and stdout, one request
    /// at a time. A request cut off by an interrupt may leave half a line
    /// either way, so the connection is not used again (`cut`).
    Stdio { io: tokio::sync::Mutex<StdioIo>, next: AtomicU64, cut: AtomicBool },
    /// Streamable HTTP: each message a POST, answered with JSON or an SSE
    /// stream, the session id carried from `initialize` on.
    Http { client: reqwest::Client, url: String, headers: BTreeMap<String, String>, session: std::sync::Mutex<Option<String>>, next: AtomicU64 },
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
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The group the server leads (`process_group(0)` below).
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for StdioIo {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Conn {
    fn open(c: &ServerConfig, cwd: &Path) -> Result<Conn, String> {
        let next = AtomicU64::new(1);
        match (c.kind.as_deref(), &c.command, &c.url) {
            (Some("http" | "streamable-http") | None, None, Some(url)) => {
                let client = crate::http::client().map_err(|e| e.message)?;
                Ok(Conn::Http { client, url: url.clone(), headers: c.headers.clone(), session: std::sync::Mutex::new(None), next })
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
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
                Ok(Conn::Stdio { io: tokio::sync::Mutex::new(StdioIo { stdin, stdout, child }), next, cut: AtomicBool::new(false) })
            }
            (kind, ..) => Err(format!("it names neither a command nor a url for its transport ({})", kind.unwrap_or("stdio"))),
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
            Conn::Stdio { io, cut, .. } => {
                let mut io = io.lock().await;
                if cut.swap(true, Ordering::AcqRel) {
                    return Err("an earlier call to this server was cut off mid-message (an interrupt, or a pipe that broke), so it is not used again this turn".into());
                }
                write_line(&mut io.stdin, &msg).await?;
                let answer = loop {
                    let mut line = String::new();
                    let read = (&mut io.stdout).take(MAX_MESSAGE).read_line(&mut line).await.map_err(|e| e.to_string())?;
                    if read == 0 {
                        return Err("the server exited".into());
                    }
                    if !line.ends_with('\n') && read as u64 == MAX_MESSAGE {
                        return Err(format!("the server sent a message over {} MiB", MAX_MESSAGE >> 20));
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
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
        let Conn::Http { client, url, headers, session, .. } = self else { unreachable!("post is HTTP's") };
        let mut req = client.post(url).header("content-type", "application/json").header("accept", "application/json, text/event-stream").header("mcp-protocol-version", MCP_VERSION);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(s) = session.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            req = req.header("mcp-session-id", s);
        }
        let mut resp = req.body(msg.to_string()).send().await.map_err(|e| e.to_string())?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.to_string());
        }
        let status = resp.status();
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
        let names: Vec<String> = discover(&cfg, &repo).into_iter().map(|s| s.name).collect();
        assert!(names.is_empty(), "the swapped-in servers were loaded: {names:?}");
        // The file as trusted loads.
        std::fs::write(repo.join(".mcp.json"), json!({"mcpServers": {"good": {"command": "good"}}}).to_string()).unwrap();
        assert_eq!(discover(&cfg, &repo).into_iter().map(|s| s.name).collect::<Vec<_>>(), ["good"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn r_tool_3_a_call_naming_a_control_character_is_refused_before_it_is_judged() {
        assert_eq!(target(&json!({"tool": "fake:echo"})).unwrap(), ("fake".into(), "echo".into()));
        for bad in ["fake:echo\u{1b}[2J", "fake:a\nb", "fa ke:echo", "fake:", ":echo", "s_:x"] {
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
        let untrusted = discover(&cfg(false), &repo);
        assert_eq!(untrusted.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["github"]);
        assert_eq!(untrusted[0].cwd, base.join("home"), "not in the repository nobody trusted");
        let trusted = discover(&cfg(true), &repo);
        let github = trusted.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(github.config.command.as_deref(), Some("github-mcp-server"), "the person's own server outranks a repository's of the same name");
        assert!(trusted.iter().any(|s| s.name == "repo" && s.cwd == repo));
        let _ = std::fs::remove_dir_all(&base);
    }
}
