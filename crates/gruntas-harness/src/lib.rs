//! gruntas's host for Claude Code and Codex sessions: what `gruntas -p`, the
//! daemon and sync run turns through. gruntas runs no model itself; every
//! turn runs inside the stock `claude` or `codex`.
//!
//! - `protocol` — the typed commands, events and log lines every client
//!   speaks; the JSON Schema in `schema/` is generated from it.
//! - `engine` — the `Engine` trait the backends implement, and the
//!   `EngineEvent`s they report.
//! - `claude` — the Claude Code backend: the user's own `claude` binary,
//!   driven over stream-json and its control protocol, whose Messages
//!   stream `claude::messages` decodes; `bridge` — gruntas's tools offered to
//!   a backend as an MCP server; `trust` — which repositories a backend may
//!   run in.
//! - `codex` — the Codex backend: the user's own `codex`, driven as `codex
//!   app-server` over JSON-RPC, with gruntas's tools as its dynamic tools.
//! - `http` — the HTTP client every request gruntas makes goes through, and
//!   `sse` the event-stream parser.
//! - `neutral` — text gruntas frames around words it did not write, made
//!   unable to close the frame (a handoff, another model's reasoning).
//! - `redact` — transcript text for a reading agent: known secret shapes
//!   redacted, then paged and framed as foreign text.
//! - `catalog` — what the models.dev cache says of a model; `effort` — the
//!   one reasoning-effort ladder, mapped per model; `toolset` — the preset a
//!   turn's context record names.
//! - `oauth` — gruntas's credentials file and the OAuth pieces its sign-ins
//!   share; `mcp_oauth` — signing in to an MCP server.
//! - `evidence` — `publish`, and the session's gruntas run; `builds` — the
//!   build slots, on `slots`, the machine-wide pools of lock files.
//! - `agents` — the model listing and where gruntas's worktrees are;
//!   `children` — how many child sessions run at once, the machine's agent
//!   slots, and the watch that frames a backend's agent for its parent;
//!   `worktree` — the git worktree a session can run in, made, readied and
//!   removed; `sandbox` — the OS sandbox a worktree's setup runs in.
//! - `budget` — what a session may spend, checked before each turn and as
//!   each call is metered.
//! - `paste_guard` — refuses a `gh` post whose body carries a bare gruntas
//!   card link, on every backend and in every mode.
//! - `tooling_guard` — refuses a worktree made by hand and a file sent to
//!   a paste site, pointing at gruntas's own, alongside the paste guard.
//! - `permissions` — Claude-Code-compatible modes, rules and approvals:
//!   the one evaluator every call a backend asks about is judged by, with
//!   the reach of a file a call names (`permissions::scope`), its globs and
//!   the hooks the settings name.
//! - `compat` — the skills a person asks for by `/name`, and the MCP
//!   servers a turn may use.
//! - `mcp` — the person's and a trusted repository's MCP servers, their
//!   tools deferred behind `mcp_search` and `mcp_call` (R-TOOL-3).
//! - `host` — executes commands, writes the log, prices the turn.
//! - `handoff` — carrying a session into a backend that did not run all of
//!   it: its own thread caught up, another account's transcript copied, or
//!   a new thread seeded with gruntas's summary (R-SWITCH-2, R-INST-4).
//! - `log` — the append-only JSONL session log and its layout on disk;
//!   `images` — the images a prompt carries, kept beside it.
//! - `project` — the log as a `gruntas_import::Source`, so gruntas.db lists
//!   gruntas's sessions beside imported ones.
//! - `instances` — the named accounts in gruntas's config; `readiness` —
//!   whether each can run a turn here, and what fixes it; `connect` —
//!   connecting one and signing it out, for the CLI and an interactive
//!   client.
//! - `headless` — `gruntas -p`.
//! - `daemon` — the per-user host daemon a session outlives the terminal
//!   in, its unix socket, and the service that keeps it up (R-HOST-1,
//!   R-HOST-2).
//!
//! Canon `engineering/harness.md` describes all of it for readers who will
//! not open the code.

pub mod agents;
pub mod ask;
pub mod bridge;
pub mod budget;
pub mod builds;
pub mod catalog;
pub mod children;
pub mod claude;
pub mod codex;
pub mod compat;
pub mod connect;
#[cfg(unix)]
pub mod daemon;
pub mod effort;
pub mod engine;
pub mod evidence;
pub mod group;
pub mod handoff;
pub mod headless;
pub mod host;
pub mod http;
pub mod images;
pub mod pasted;
pub mod upload;
pub mod usage;
pub mod instances;
pub mod keys;
pub mod log;
pub mod mcp;
pub mod mcp_oauth;
pub mod neutral;
pub mod redact;
pub mod oauth;
pub mod paste_guard;
pub mod permissions;
pub mod project;
/// Declared in `gruntas-client`, which every client links without the
/// engine (R-CLIENT-1); re-exported so `crate::protocol` paths hold.
pub use gruntas_client::protocol;
pub mod readiness;
#[cfg(unix)]
pub mod relay;
pub mod sandbox;
pub mod schema;
pub mod slots;
pub mod steer;
#[cfg(unix)]
pub mod sync;
pub mod sse;
pub mod tooling_guard;
pub mod toolset;
pub mod trust;
// The archive compresses with ruzstd, which links on unix only, where
// `gruntas sessions` runs.
#[cfg(unix)]
pub mod vintage;
pub mod worktree;
