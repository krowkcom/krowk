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
use krowk_harness::protocol::{Command, LiveEvent, ModelRef, RunResult, StreamLine};
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

/// The error code of an image that could not be sent ahead of its prompt:
/// the prompt goes back to the composer, as for any image refused.
pub const IMAGE_NOT_SENT: &str = "image_not_sent";

/// The `turn_id` of the notice that tells the status line which image is
/// going (`Sending image 2 of 3…`); it is not a turn's.
pub const SENDING: &str = "sending-images";

/// The most a message may carry on the wire: the daemon's and the relay's cap.
const MESSAGE_CAP: usize = 4 * 1024 * 1024;

/// A host as the images' sending needs to know it.
struct Peer<'a> {
    name: &'a str,
    krowk_version: &'a str,
    /// The protocol version it announced, when it did.
    protocol: Option<u32>,
}

/// The status line's text, if `line` is the notice that carries it.
pub fn sending(line: &StreamLine) -> Option<&str> {
    match line {
        StreamLine::Live(LiveEvent::Notice { turn_id, text, .. }) if turn_id == SENDING => Some(text),
        _ => None,
    }
}

fn say(out: &mpsc::Sender<StreamLine>, text: String) {
    let _ = out.try_send(StreamLine::Live(LiveEvent::Notice { session_id: String::new(), turn_id: SENDING.into(), text }));
}

/// Sends a prompt's or steer's images ahead of it, one `putImage` after
/// another, and returns the command naming them by sha256 with no bytes
/// (D2). Toward a host of an older protocol it returns the command as it
/// is, images inline, when it fits one message, and refuses before sending
/// anything when it does not (R-IMG-7).
async fn upload_images<F, Fut>(cmd: Command, host: &Peer<'_>, out: &mpsc::Sender<StreamLine>, send: F) -> Result<Command, EngineError>
where
    F: Fn(Command, mpsc::Sender<StreamLine>) -> Fut,
    Fut: std::future::Future<Output = Result<Option<RunResult>, EngineError>>,
{
    let (session, text_len, images) = match &cmd {
        Command::Prompt { session_id, text, images, .. } => (session_id.clone(), text.len(), images),
        Command::Steer { session_id, text, images } => (Some(session_id.clone()), text.len(), images),
        _ => return Ok(cmd),
    };
    if images.iter().all(|i| i.data.is_empty()) {
        return Ok(cmd);
    }
    if host.protocol.is_some_and(|p| p < 2) {
        let total = text_len + images.iter().map(|i| i.data.len()).sum::<usize>();
        // Headroom for the rest of the message.
        return match total > MESSAGE_CAP - 64 * 1024 {
            false => Ok(cmd),
            true => Err(EngineError::new(IMAGE_NOT_SENT, format!("{} runs krowk {}, which takes a prompt's images only in one message, and these come to {:.1} MB; update krowk there or send fewer images", host.name, host.krowk_version, total as f64 / (1024.0 * 1024.0)))),
        };
    }
    let n = images.len();
    for (k, i) in images.iter().enumerate() {
        say(out, format!("Sending image {} of {n}…", k + 1));
        let Some(sha256) = i.sha256.clone() else { continue };
        let put = Command::PutImage { session_id: session.clone(), sha256, media_type: i.media_type.clone(), data: i.data.clone() };
        let (sink, _) = mpsc::channel(1);
        if let Err(e) = send(put, sink).await {
            say(out, String::new());
            return Err(EngineError::new(IMAGE_NOT_SENT, format!("image #{} was not sent: {}", i.number, e.message)));
        }
    }
    say(out, String::new());
    let by_name = |images: &[krowk_harness::protocol::ImageInput]| images.iter().map(|i| krowk_harness::protocol::ImageInput { data: if i.sha256.is_some() { String::new() } else { i.data.clone() }, ..i.clone() }).collect::<Vec<_>>();
    Ok(match cmd {
        Command::Prompt { session_id, text, images, model, permission_mode, toolset, effort, budget } => Command::Prompt { session_id, text, images: by_name(&images), model, permission_mode, toolset, effort, budget },
        Command::Steer { session_id, text, images } => Command::Steer { session_id, text, images: by_name(&images) },
        other => other,
    })
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
    use super::{reads_images, sending, upload_images, Peer, IMAGE_NOT_SENT};
    use krowk_harness::protocol::{Command, ImageInput, PermissionMode};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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
        Command::Prompt { session_id: None, text: "hi".into(), images, model: None, permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None }
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
