//! What a repository set up for Claude Code, Codex or Cursor brings to a
//! native krowk turn, read unchanged (R-COMPAT-1): its instruction files
//! (`instructions`), its skills (`skills`), and the hooks its settings name
//! (`crate::hooks`, loaded with the permission rules).
//!
//! A backend reads its own vendor's files itself — Claude Code its
//! `CLAUDE.md`, skills and hooks, Codex its `AGENTS.md` — so none of this is
//! handed to one twice.

pub mod instructions;
pub mod skills;

use crate::hooks::Hooks;
use crate::permissions::Config;
use std::path::{Path, PathBuf};

pub use instructions::Instruction;
pub use skills::Skill;

/// One turn's instructions, skills and hooks.
#[derive(Debug, Clone, Default)]
pub struct Compat {
    pub instructions: Vec<Instruction>,
    pub skills: Vec<Skill>,
    pub hooks: Hooks,
    /// The repository root: hooks' `CLAUDE_PROJECT_DIR`.
    pub project_dir: PathBuf,
    /// The session's log, as hooks' `transcript_path`.
    pub transcript: String,
    /// `SessionStart`'s source when this turn is the first a host runs of
    /// the session: `startup` for a new one, `resume` for one resumed.
    pub session_start: Option<&'static str>,
    /// The MCP servers the turn may use, started on first use and shared
    /// with its subagents (`crate::mcp`).
    pub mcp: std::sync::Arc<crate::mcp::Mcp>,
}

impl Compat {
    /// Everything that applies in `cwd`, with the hooks the settings named.
    pub fn load(cfg: &Config, cwd: &Path, hooks: Hooks) -> Compat {
        let project_dir = crate::trust::root(cwd);
        let mcp = std::sync::Arc::new(crate::mcp::Mcp::new(crate::mcp::discover(cfg, cwd)));
        Compat { instructions: instructions::discover(cfg, cwd), skills: skills::discover(cfg, cwd), hooks, project_dir, mcp, ..Compat::default() }
    }

    /// What the native system prompt carries after its own few lines: the
    /// instructions whole, and each skill's name and description — never a
    /// skill's body, which the `skill` tool loads when it is used.
    pub fn prompt(&self) -> String {
        let mut out = String::new();
        out.push_str(&instructions::render(&self.instructions));
        out.push_str(&skills::render(&self.skills));
        out
    }
}
