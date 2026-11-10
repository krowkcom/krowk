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
pub use krowk_harness::upload::{sending, upload_images, Peer, IMAGE_NOT_SENT, SENDING};
use krowk_harness::instances::{Asked, Registry};
use krowk_harness::protocol::{Command, McpServer, ModelRef, RunResult, StreamLine};
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
    fn protocol_version(&self) -> u32 {
        match *self {}
    }
    async fn renew(&self) -> bool {
        match *self {}
    }
    fn watch(&self) -> broadcast::Receiver<StreamLine> {
        match *self {}
    }
    pub async fn status(&self) -> Result<krowk_harness::protocol::HostStatus, EngineError> {
        match *self {}
    }
    pub async fn mcp_status(&self, _: &str) -> Result<Vec<McpServer>, EngineError> {
        match *self {}
    }
    pub async fn mcp_reconnect(&self, _: &str, _: &str) -> Result<(), EngineError> {
        match *self {}
    }
    pub async fn mcp_sign_in_start(&self, _: &str, _: &str) -> Result<String, EngineError> {
        match *self {}
    }
    pub async fn mcp_sign_in_finish(&self, _: &str, _: &str, _: &str) -> Result<(), EngineError> {
        match *self {}
    }
    pub async fn mcp_sign_out(&self, _: &str, _: &str) -> Result<(), EngineError> {
        match *self {}
    }
    pub fn attach_later(&self, _: &str, _: Option<&str>) {
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
                    return Err(EngineError::new(HOST_READS_NO_IMAGES, format!("the host daemon runs krowk {version}, which drops a prompt's images, and is replaced with this krowk once no turn or background agent runs there — send it again then (a daemon run as a service: restart it)")));
                }
                let host = Peer { name: "the host daemon", krowk_version: &version, protocol: Some(client.protocol_version()) };
                let cmd = upload_images(cmd, &host, &out, |c, o| client.execute(c, o)).await?;
                client.execute(cmd, out).await
            }
            // The host's version is not known to a viewer: its images go
            // ahead as a host of this protocol takes them.
            Link::Synced { client, .. } => {
                let host = Peer { name: "the host", krowk_version: "", protocol: None };
                let cmd = upload_images(cmd, &host, &out, |c, o| client.execute(c, o)).await?;
                client.execute(cmd, out).await
            }
        }
    }

    /// The session's MCP servers as its host holds them. A daemon of an
    /// older protocol, or a session on another machine, cannot say: the
    /// message to show in the view's place.
    pub async fn mcp_status(&self, session_id: &str) -> Result<Vec<McpServer>, String> {
        match self {
            Link::Local(h) => Ok(h.mcp_servers(session_id)),
            Link::Remote { client, .. } if let Err(m) = mcp_gate(client.protocol_version(), &client.krowk_version()) => Err(m),
            Link::Remote { client, .. } => client.mcp_status(session_id).await.map_err(|e| e.message),
            Link::Synced { .. } => Err("/mcp is the host's: this session runs on another machine".into()),
        }
    }

    /// Begins signing in to the session's MCP server on its host: the page
    /// to open, and whether the host listens for the browser itself (a host
    /// in this process does; a daemon is asked for the pasted address).
    pub async fn mcp_sign_in_start(&self, session_id: &str, server: &str) -> Result<(String, bool), String> {
        match self {
            Link::Local(h) => h.mcp_sign_in_start(session_id, server, true).await.map_err(|e| e.message),
            Link::Remote { client, .. } if let Err(m) = mcp_gate(client.protocol_version(), &client.krowk_version()) => Err(m),
            Link::Remote { client, .. } => client.mcp_sign_in_start(session_id, server).await.map(|u| (u, false)).map_err(|e| e.message),
            Link::Synced { .. } => Err("/mcp is the host's: this session runs on another machine".into()),
        }
    }

    /// Finishes it with the pasted landing address, which is never echoed.
    pub async fn mcp_sign_in_finish(&self, session_id: &str, server: &str, landing: &str) -> Result<(), String> {
        match self {
            Link::Local(h) => h.mcp_sign_in_finish(session_id, server, landing).await.map_err(|e| e.message),
            Link::Remote { client, .. } if let Err(m) = mcp_gate(client.protocol_version(), &client.krowk_version()) => Err(m),
            Link::Remote { client, .. } => client.mcp_sign_in_finish(session_id, server, landing).await.map_err(|e| e.message),
            Link::Synced { .. } => Err("/mcp is the host's: this session runs on another machine".into()),
        }
    }

    /// Signs the session's MCP server out.
    pub async fn mcp_sign_out(&self, session_id: &str, server: &str) -> Result<(), String> {
        match self {
            Link::Local(h) => h.mcp_sign_out(session_id, server).await.map_err(|e| e.message).and_then(|f| f.then_some(()).ok_or_else(|| format!("no MCP server {server}"))),
            Link::Remote { client, .. } if let Err(m) = mcp_gate(client.protocol_version(), &client.krowk_version()) => Err(m),
            Link::Remote { client, .. } => client.mcp_sign_out(session_id, server).await.map_err(|e| e.message),
            Link::Synced { .. } => Err("/mcp is the host's: this session runs on another machine".into()),
        }
    }

    /// Starts the session's MCP server again; it connects in the
    /// background, so ask `mcp_status` after.
    pub async fn mcp_reconnect(&self, session_id: &str, server: &str) -> Result<(), String> {
        match self {
            Link::Local(h) => h.mcp_reconnect(session_id, server).await.then_some(()).ok_or_else(|| format!("no MCP server {server}")),
            Link::Remote { client, .. } if let Err(m) = mcp_gate(client.protocol_version(), &client.krowk_version()) => Err(m),
            Link::Remote { client, .. } => client.mcp_reconnect(session_id, server).await.map_err(|e| e.message),
            Link::Synced { .. } => Err("/mcp is the host's: this session runs on another machine".into()),
        }
    }

    /// The daemon replaced with one of this krowk when it is of an older
    /// one and can go (`Remote::renew`): answers whether it was. `quiet`
    /// is the caller's word that nothing of its own runs there in the
    /// background, which the daemon may not know to count.
    pub async fn renew(&self, quiet: bool) -> bool {
        match self.remote() {
            Some(c) if quiet => c.renew().await,
            _ => false,
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

    /// The background jobs `shutdown` stops, as (session, id): this
    /// process's. The daemon's run on.
    pub fn running_jobs(&self) -> Vec<(String, String)> {
        self.host().running_jobs()
    }
}

/// The first krowk whose host answers `/mcp` (protocol 3).
const MCP_SINCE: &str = "0.16.0";

/// What the view says in its place against a host of an older protocol.
fn mcp_gate(protocol: u32, host_version: &str) -> Result<(), String> {
    match protocol >= 3 {
        true => Ok(()),
        false => Err(format!("/mcp needs krowk {MCP_SINCE} or later on the host daemon, which runs {host_version}")),
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
    use super::{mcp_gate, reads_images, sending, upload_images, Peer, IMAGE_NOT_SENT};
    use krowk_harness::protocol::{Command, ImageInput, PermissionMode};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    #[test]
    fn r_mcp_21_a_host_of_protocol_2_gets_the_upgrade_message_in_place_of_the_view() {
        assert_eq!(mcp_gate(2, "0.15.0").unwrap_err(), "/mcp needs krowk 0.16.0 or later on the host daemon, which runs 0.15.0");
        assert!(mcp_gate(krowk_harness::protocol::PROTOCOL_VERSION, "0.16.0").is_ok());
    }

    #[test]
    fn a_daemon_before_0_13_drops_images() {
        assert!(!reads_images("0.12.1"));
        assert!(reads_images("0.13.0"));
        assert!(reads_images("0.13.1-dev"));
        assert!(reads_images("1.0.0"));
        assert!(reads_images("dev"), "a version that does not parse is taken to read them");
    }

    fn prompt(sizes: &[usize]) -> Command {
        let images = sizes.iter().enumerate().map(|(n, s)| ImageInput { number: n as u32 + 1, media_type: "image/png".into(), data: "A".repeat(*s), sha256: Some(format!("{n:064}")) }).collect();
        Command::Prompt { session_id: None, text: "hi".into(), images, model: None, permission_mode: PermissionMode::Default, toolset: None, effort: None, rollover: None, budget: None }
    }

    async fn upload(cmd: Command, protocol: Option<u32>) -> (Result<Command, krowk_harness::engine::EngineError>, Vec<Command>, Vec<String>) {
        let sent = Mutex::new(Vec::new());
        let (out, mut said) = mpsc::channel(16);
        let peer = Peer { name: "the host daemon", krowk_version: "0.20.0", protocol };
        let r = upload_images(cmd, &peer, &out, |c, _| {
            sent.lock().unwrap().push(c);
            async { Ok(None) }
        })
        .await;
        let mut lines = Vec::new();
        while let Ok(l) = said.try_recv() {
            lines.extend(sending(&l).map(String::from));
        }
        (r, sent.into_inner().unwrap(), lines)
    }

    #[tokio::test]
    async fn each_image_goes_ahead_as_its_own_command_and_the_prompt_names_them() {
        let (r, sent, lines) = upload(prompt(&[3 << 20, 3 << 20]), Some(2)).await;
        assert!(sent.iter().all(|c| matches!(c, Command::PutImage { .. })) && sent.len() == 2);
        let Ok(Command::Prompt { images, .. }) = r else { panic!("{r:?}") };
        assert!(images.iter().all(|i| i.data.is_empty() && i.sha256.is_some()));
        assert_eq!(lines, ["Sending image 1 of 2…", "Sending image 2 of 2…", ""]);
    }

    #[tokio::test]
    async fn toward_an_older_host_a_prompt_that_fits_goes_inline_and_one_that_does_not_is_refused() {
        let (r, sent, _) = upload(prompt(&[1 << 20]), Some(1)).await;
        assert!(sent.is_empty());
        assert!(matches!(r, Ok(Command::Prompt { images, .. }) if images[0].data.len() == 1 << 20));
        let (r, sent, _) = upload(prompt(&[3 << 20, 3 << 20]), Some(1)).await;
        assert!(sent.is_empty(), "nothing sent");
        let e = r.unwrap_err();
        assert_eq!(e.code, IMAGE_NOT_SENT);
        assert_eq!(e.message, "the host daemon runs krowk 0.20.0, which takes a prompt's images only in one message, and these come to 6.0 MB; update krowk there or send fewer images");
    }
}
