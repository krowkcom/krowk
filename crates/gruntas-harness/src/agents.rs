//! What a host is told of the person's machine for its sessions: the
//! models.dev listing a bare model id is routed by, and where krowk keeps
//! its worktrees. krowk's own agent definitions went with its `subagent`
//! tool; Claude Code and Codex read their own.

use crate::catalog::Listed;
use std::path::PathBuf;
use std::sync::Arc;

/// Every model the catalog lists for a provider, for routing a bare model
/// id: supplied by the caller, which owns the models.dev cache.
pub type Models = Arc<dyn Fn(&str) -> Vec<Listed> + Send + Sync>;

/// The model listing, and where krowk's worktrees are.
#[derive(Clone)]
pub struct AgentsConfig {
    pub models: Models,
    /// Where krowk makes its worktrees (`gruntas_api::home::worktrees_root`):
    /// what WT9's daily prune clears.
    pub worktrees: Option<PathBuf>,
}

impl AgentsConfig {
    /// A catalog that lists nothing, and no worktrees.
    pub fn none() -> AgentsConfig {
        AgentsConfig { models: Arc::new(|_| Vec::new()), worktrees: None }
    }
}
