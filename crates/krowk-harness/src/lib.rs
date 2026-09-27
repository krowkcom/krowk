//! krowk's own agent engine: the model loop behind `krowk -p`.
//!
//! - `protocol` — the typed commands, events and log lines every client
//!   speaks; the JSON Schema in `schema/` is generated from it.
//! - `engine` — the `Engine` trait native providers and vendor backends
//!   implement, and the `EngineEvent`s they report.
//! - `native` — krowk's own loop over a `ModelClient`, one per wire API:
//!   `anthropic` (Messages), `openai` (Responses), `chat` (Chat
//!   Completions: xAI, OpenRouter and anything compatible). `http` and
//!   `sse` are what the three share.
//! - `catalog` — what the models.dev cache says of a model; `effort` — the
//!   one reasoning-effort ladder, mapped per model.
//! - `oauth` — the SuperGrok login and its tokens.
//! - `claude` — the Claude Code backend: the user's own `claude` binary,
//!   driven over stream-json and its control protocol; `bridge` — krowk's
//!   tools offered to a backend as an MCP server; `trust` — which
//!   repositories a backend may run in.
//! - `codex` — the Codex backend: the user's own `codex`, driven as `codex
//!   app-server` over JSON-RPC, with krowk's tools as its dynamic tools.
//! - `tools` — read, write, the edit tools, bash, grep and glob;
//!   `evidence` — `publish`, and the session's krowk run; `todo` —
//!   `todo_write` and the stale-list reminder.
//! - `subagent` — the `subagent` tool: child sessions with their own
//!   context, model and tools, fanned out in parallel; `agents` — the agent
//!   definitions they run, krowk's and Claude Code's.
//! - `budget` — what a session may spend, checked before every model call.
//! - `permissions` — Claude-Code-compatible modes, rules and approvals:
//!   the one evaluator every call is judged by, native or a backend's.
//! - `compat` — the instructions (`AGENTS.md`, `CLAUDE.md`, `.cursor/rules`)
//!   and skills a native turn reads; `hooks` — Claude-format command hooks.
//! - `toolset` — the preset registry: which edit tool a model is offered.
//! - `host` — executes commands, writes the log, prices the turn.
//! - `handoff` — carrying a session into a backend that did not run all of
//!   it: its own thread caught up, another account's transcript copied, or
//!   a new thread seeded with krowk's summary (R-SWITCH-2, R-INST-4).
//! - `log` — the append-only JSONL session log and its layout on disk.
//! - `project` — the log as a `krowk_import::Source`, so krowk.db lists
//!   native sessions beside imported ones.
//! - `instances` — the named provider instances in krowk's config;
//!   `readiness` — whether each can run a turn here, and what fixes it.
//! - `headless` — `krowk -p`.
//!
//! Canon `engineering/harness.md` describes all of it for readers who will
//! not open the code.

pub mod agents;
pub mod anthropic;
pub mod bridge;
pub mod budget;
pub mod catalog;
pub mod chat;
pub mod claude;
pub mod codex;
pub mod compat;
pub mod effort;
pub mod engine;
pub mod evidence;
pub mod group;
pub mod handoff;
pub mod headless;
pub mod hooks;
pub mod host;
pub mod http;
pub mod instances;
pub mod log;
pub mod native;
pub mod oauth;
pub mod openai;
pub mod permissions;
pub mod project;
pub mod protocol;
pub mod readiness;
pub mod schema;
pub mod subagent;
pub mod sse;
pub mod todo;
pub mod tools;
pub mod toolset;
pub mod trust;
