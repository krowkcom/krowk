//! Where the TUI's sessions run: the host daemon over its unix socket, by
//! default, or a `Host` in this process. The TUI speaks to either through
//! this seam and nothing else (R-PROTO-1), so what it draws is the same
//! protocol whichever it is.
//!
//! Over the socket the turns are the daemon's: closing the terminal leaves
//! them running, and reopening krowk follows them again (R-HOST-1). The
//! TUI still keeps a `Host` of its own for what asks nothing of a session
//! — the instances it lists, and routing a bare model id, which asks the
//! vendors from here — and a connection made here is read again by the
//! daemon (`reload`).

use krowk_harness::daemon::client::Client;
use krowk_harness::engine::EngineError;
use krowk_harness::host::Host;
use krowk_harness::instances::{Asked, Registry};
use krowk_harness::protocol::{Command, ModelRef, RunResult, StreamLine};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

pub enum Link {
    Local(Host),
    Remote { host: Host, client: Client },
}

impl Link {
    fn host(&self) -> &Host {
        match self {
            Link::Local(h) | Link::Remote { host: h, .. } => h,
        }
    }

    /// The daemon's client, when the sessions run there.
    pub fn remote(&self) -> Option<&Client> {
        match self {
            Link::Remote { client, .. } => Some(client),
            Link::Local(_) => None,
        }
    }

    pub fn registry(&self) -> Arc<Registry> {
        self.host().registry()
    }

    pub async fn route_model(&self, asked: Option<&Asked>, current: Option<&ModelRef>, cwd: &std::path::Path) -> Result<ModelRef, EngineError> {
        self.host().route_model(asked, current, cwd).await
    }

    /// The instances read again after a connection or a sign-out: here, and
    /// in the daemon, whose next turns run on them.
    pub fn set_registry(&self, registry: Registry, changed: Option<&str>) {
        self.host().set_registry(registry, changed);
        if let Some(c) = self.remote() {
            c.reload_later(changed.map(String::from), None);
        }
    }

    pub fn set_registry_renamed(&self, registry: Registry, from: &str, to: &str) {
        self.host().set_registry_renamed(registry, from, to);
        if let Some(c) = self.remote() {
            c.reload_later(None, Some((from.to_string(), to.to_string())));
        }
    }

    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        match self {
            Link::Local(h) => h.execute(cmd, out).await,
            Link::Remote { client, .. } => client.execute(cmd, out).await,
        }
    }

    /// What arrives between turns: a backend's agents, a turn it began.
    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        match self {
            Link::Local(h) => h.watch(),
            Link::Remote { client, .. } => client.watch(),
        }
    }

    /// Lets this process's backend processes go. The daemon's stay: they are
    /// its sessions', which outlive this TUI.
    pub async fn shutdown(&self) {
        self.host().shutdown().await;
    }
}
