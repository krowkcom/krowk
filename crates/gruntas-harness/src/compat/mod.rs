//! What gruntas brings to a turn beyond the backend's own reading: the
//! skills a person can ask for by `/name` (`skills`), and the MCP servers
//! the backend reaches through gruntas's bridge (`crate::mcp`).
//!
//! A backend reads its own vendor's files itself — Claude Code its
//! `CLAUDE.md`, skills and hooks, Codex its `AGENTS.md` — so none of those
//! is handed to one twice.

pub mod skills;

use crate::permissions::Config;
use std::path::Path;

pub use skills::Skill;

/// One turn's skills and MCP servers.
#[derive(Debug, Clone, Default)]
pub struct Compat {
    pub skills: Vec<Skill>,
    /// The MCP servers the turn may use, started on first use
    /// (`crate::mcp`).
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
}

impl Compat {
    /// Everything that applies in `cwd`.
    pub fn load(cfg: &Config, cwd: &Path) -> Compat {
        // What it leaves out for a name or config it cannot use was said
        // only on gruntas's own turns.
        let (servers, _) = crate::mcp::discover(cfg, cwd);
        let mcp = std::sync::Arc::new(crate::mcp::Mcp::new(servers));
        Compat { skills: skills::discover(cfg, cwd), mcp }
    }
}
