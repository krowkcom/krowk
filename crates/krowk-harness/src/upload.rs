//! Sending a prompt's or steer's images ahead of it, one `put_image` after
//! another: what the TUI's link, `krowk -p` on a daemon and the typed line
//! of `krowk sync attach` share (R-IMG-2, R-IMG-5, R-IMG-7).

use crate::engine::EngineError;
use crate::protocol::{Command, LiveEvent, RunResult, StreamLine};
use tokio::sync::mpsc;

/// The error code of an image that could not be sent ahead of its prompt:
/// the prompt goes back to the composer, as for any image refused.
pub const IMAGE_NOT_SENT: &str = "image_not_sent";

/// The `turn_id` of the notice that tells the status line which image is
/// going (`Sending image 2 of 3…`); it is not a turn's.
pub const SENDING: &str = "sending-images";

/// The most a message may carry on the wire: the daemon's and the relay's cap.
pub const MESSAGE_CAP: usize = 4 * 1024 * 1024;

/// A host as the images' sending needs to know it.
pub struct Peer<'a> {
    pub name: &'a str,
    pub krowk_version: &'a str,
    /// The protocol version it announced, when it did.
    pub protocol: Option<u32>,
}

/// The status line's text, if `line` is the notice that carries it.
pub fn sending(line: &StreamLine) -> Option<&str> {
    match line {
        StreamLine::Live(LiveEvent::Notice { turn_id, text, .. }) if turn_id == SENDING => Some(text),
        _ => None,
    }
}

pub fn say(out: &mpsc::Sender<StreamLine>, text: String) {
    let _ = out.try_send(StreamLine::Live(LiveEvent::Notice { session_id: String::new(), turn_id: SENDING.into(), text }));
}

/// Sends a prompt's or steer's images ahead of it, one `putImage` after
/// another, and returns the command naming them by sha256 with no bytes
/// (D2). Toward a host of an older protocol it returns the command as it
/// is, images inline, when it fits one message, and refuses before sending
/// anything when it does not (R-IMG-7).
pub async fn upload_images<F, Fut>(cmd: Command, host: &Peer<'_>, out: &mpsc::Sender<StreamLine>, send: F) -> Result<Command, EngineError>
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
            // A host that cannot be reached is told as that, for the
            // reachability check; the rest send the prompt back.
            let code = if e.code == "network_unreachable" { e.code.as_str() } else { IMAGE_NOT_SENT };
            return Err(EngineError::new(code, format!("image #{} was not sent: {}", i.number, e.message)));
        }
    }
    say(out, String::new());
    let by_name = |images: &[crate::protocol::ImageInput]| images.iter().map(|i| crate::protocol::ImageInput { data: if i.sha256.is_some() { String::new() } else { i.data.clone() }, ..i.clone() }).collect::<Vec<_>>();
    Ok(match cmd {
        Command::Prompt { session_id, text, images, model, permission_mode, toolset, effort, budget } => Command::Prompt { session_id, text, images: by_name(&images), model, permission_mode, toolset, effort, budget },
        Command::Steer { session_id, text, images } => Command::Steer { session_id, text, images: by_name(&images) },
        other => other,
    })
}
