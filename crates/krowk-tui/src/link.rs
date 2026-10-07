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

#[cfg(unix)]
use krowk_harness::daemon::remote::Remote;
use krowk_harness::engine::EngineError;
use krowk_harness::host::Host;
use krowk_harness::instances::{Asked, Registry};
use krowk_harness::protocol::{Command, ModelRef, RunResult, StreamLine};
pub use crate::synced::SyncLink;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// The daemon's client; the daemon is unix-only for now, so elsewhere there
/// is none and every session runs in the process.
#[cfg(not(unix))]
pub enum Remote {}

#[cfg(not(unix))]
impl Remote {
    fn leave(&self, _: &str) {
        match *self {}
    }
    fn take_note(&self) -> Option<String> {
        match *self {}
    }
    fn reload_later(&self, _: Option<String>, _: Option<(String, String)>) {
        match *self {}
    }
    pub async fn follow(&self, _: &str, _: Option<&str>, _: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        match *self {}
    }
    async fn execute(&self, _: Command, _: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        match *self {}
    }
    fn krowk_version(&self) -> String {
        match *self {}
    }
    fn watch(&self) -> broadcast::Receiver<StreamLine> {
        match *self {}
    }
}

pub enum Link {
    Local(Host),
    Remote { host: Host, client: Remote },
    /// A session another machine runs, followed through sync (D11).
    Synced { host: Host, client: SyncLink },
}

impl Link {
    fn host(&self) -> &Host {
        match self {
            Link::Local(h) | Link::Remote { host: h, .. } | Link::Synced { host: h, .. } => h,
        }
    }

    /// The daemon's client, when the sessions run there.
    pub fn remote(&self) -> Option<&Remote> {
        match self {
            Link::Remote { client, .. } => Some(client),
            Link::Local(_) | Link::Synced { .. } => None,
        }
    }

    /// The sync viewer's link, when the session runs on another machine.
    pub fn synced(&self) -> Option<&SyncLink> {
        match self {
            Link::Synced { client, .. } => Some(client),
            Link::Local(_) | Link::Remote { .. } => None,
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
            Link::Remote { client, .. } => {
                let images = matches!(&cmd, Command::Prompt { images, .. } | Command::Steer { images, .. } if !images.is_empty());
                let version = client.krowk_version();
                if images && !reads_images(&version) {
                    return Err(EngineError::new(HOST_READS_NO_IMAGES, format!("the host daemon runs krowk {version}, which drops a prompt's images — `krowk host stop` once its sessions are done, then send it again")));
                }
                client.execute(cmd, out).await
            }
            Link::Synced { client, .. } => client.execute(cmd, out).await,
        }
    }

    /// Stops following `session_id` in the daemon: the TUI moved on.
    pub fn leave(&self, session_id: &str) {
        if let Some(c) = self.remote() {
            c.leave(session_id);
        }
    }

    /// Something to tell the person about the daemon, once: a reconnection.
    pub fn take_note(&self) -> Option<String> {
        self.remote().and_then(|c| c.take_note())
    }

    /// What arrives between turns: a backend's agents, a turn it began.
    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        match self {
            Link::Local(h) => h.watch(),
            Link::Remote { client, .. } => client.watch(),
            Link::Synced { client, .. } => client.watch(),
        }
    }

    /// Lets this process's backend processes go. The daemon's stay: they are
    /// its sessions', which outlive this TUI.
    pub async fn shutdown(&self) {
        self.host().shutdown().await;
    }
}

/// A prompt the host daemon would lose its images from, refused here: the
/// prompt goes back, images and all.
pub const HOST_READS_NO_IMAGES: &str = "host_reads_no_images";

/// Whether a daemon of krowk `version` reads a prompt's images. One before
/// 0.13.0 takes them as a field it does not know and drops it, and the
/// model is sent each `[Image #N]` with no image. A version that does not
/// parse is taken to.
fn reads_images(version: &str) -> bool {
    let mut parts = version.split(['.', '-', '+']).map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (0, 13),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::reads_images;

    #[test]
    fn a_daemon_before_0_13_drops_images() {
        assert!(!reads_images("0.12.1"));
        assert!(reads_images("0.13.0"));
        assert!(reads_images("0.13.1-dev"));
        assert!(reads_images("1.0.0"));
        assert!(reads_images("dev"), "a version that does not parse is taken to read them");
    }
}
