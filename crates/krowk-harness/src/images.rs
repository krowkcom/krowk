//! The images a person attaches to a prompt. A client sends their bytes
//! with the prompt (`ImageInput`), so a host on another machine gets them
//! too; the host checks them, keeps them in the session's `images/`
//! directory beside its log, and logs only a reference (`ImageRef`). A
//! native turn reads them back for every model call, a backend is handed
//! the file.
//!
//! A client is expected to have made them small enough for any provider
//! already (the TUI does, as it pastes them); the host holds them to the
//! limits below and never resizes.

use crate::engine::EngineError;
use crate::protocol::{ImageInput, ImageRef};
use base64::Engine as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory in a session's own.
pub const DIR: &str = "images";

/// The most bytes one image may have: what the providers take of one, with
/// room for its base64.
pub const MAX_BYTES: usize = 5 * 1024 * 1024 * 3 / 4;

/// The most images one prompt or steer may carry.
pub const MAX_IMAGES: usize = 20;

/// An image checked and decoded, not yet kept.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub number: u32,
    pub media_type: &'static str,
    pub bytes: Arc<[u8]>,
}

/// The format the bytes say they are, by their first bytes alone: the four
/// every provider reads.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        _ => "webp",
    }
}

fn refused(message: String) -> EngineError {
    EngineError::new("bad_image", message)
}

/// Checks what a client sent: base64 that decodes, to bytes of the format
/// it names, within the limits. Refused whole, before any turn starts.
pub fn decode(inputs: &[ImageInput]) -> Result<Vec<Decoded>, EngineError> {
    if inputs.len() > MAX_IMAGES {
        return Err(refused(format!("{} images were attached, and a prompt takes at most {MAX_IMAGES}", inputs.len())));
    }
    inputs
        .iter()
        .map(|i| {
            let n = i.number;
            let bytes = base64::engine::general_purpose::STANDARD.decode(i.data.as_bytes()).map_err(|_| refused(format!("[Image #{n}] is not base64")))?;
            if bytes.len() > MAX_BYTES {
                return Err(refused(format!("[Image #{n}] is {} KB, and an image may be at most {} KB", bytes.len() / 1024, MAX_BYTES / 1024)));
            }
            match sniff(&bytes) {
                Some(t) if t == i.media_type => Ok(Decoded { number: n, media_type: t, bytes: bytes.into() }),
                Some(t) => Err(refused(format!("[Image #{n}] says it is {} but is {t}", i.media_type))),
                None => Err(refused(format!("[Image #{n}] is not a PNG, JPEG, GIF or WebP image"))),
            }
        })
        .collect()
}

/// Keeps the images in `session_dir`'s `images/`, private to the person,
/// each under a name of its own: two clients may both send an `[Image #1]`.
pub fn save(session_dir: &Path, images: &[Decoded]) -> Result<Vec<ImageRef>, EngineError> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    let dir = session_dir.join(DIR);
    let failed = |e: std::io::Error| EngineError::new("log_failed", format!("could not keep an image in {}: {e}", dir.display()));
    crate::log::private_dir(&dir).map_err(failed)?;
    images
        .iter()
        .map(|i| {
            let file = format!("{}-{}.{}", i.number, krowk_store::new_id(), extension(i.media_type));
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let mut f = o.open(dir.join(&file)).map_err(failed)?;
            std::io::Write::write_all(&mut f, &i.bytes).map_err(failed)?;
            Ok(ImageRef { number: i.number, media_type: i.media_type.to_string(), file })
        })
        .collect()
}

/// Where a logged image's file is, or none for a name that is not a plain
/// file name: a log is read, never trusted to point elsewhere.
pub fn path(session_dir: &Path, r: &ImageRef) -> Option<PathBuf> {
    let plain = !r.file.is_empty() && !r.file.starts_with('.') && !r.file.contains(['/', '\\', '\0']);
    plain.then(|| session_dir.join(DIR).join(&r.file))
}

/// The images a model call is sent, by file name, as base64: read once a
/// turn, so a turn of many calls reads each file once. One gone or
/// unreadable is left out, and the request says so in its place.
pub type Loaded = HashMap<String, Arc<str>>;

/// Reads every image `refs` names that `loaded` does not hold yet.
pub fn load<'a>(session_dir: &Path, refs: impl IntoIterator<Item = &'a ImageRef>, loaded: &mut Loaded) {
    for r in refs {
        if loaded.contains_key(&r.file) {
            continue;
        }
        if let Some(bytes) = path(session_dir, r).and_then(|p| std::fs::read(p).ok())
            && sniff(&bytes) == Some(r.media_type.as_str())
        {
            loaded.insert(r.file.clone(), base64::engine::general_purpose::STANDARD.encode(bytes).into());
        }
    }
}

/// The label a provider is sent before an image, so the model can tell
/// which `[Image #N]` the text means; `missing` when the file is gone.
pub fn label(r: &ImageRef, missing: bool) -> String {
    if missing { format!("[Image #{}: the file is gone]", r.number) } else { format!("[Image #{}]", r.number) }
}

/// What a native provider is sent for each image an item carries: the
/// label, and the base64 when the bytes were read; a label alone says the
/// file is gone.
pub fn sent<'a>(refs: &'a [ImageRef], loaded: &'a Loaded) -> impl Iterator<Item = (&'a ImageRef, String, Option<&'a str>)> {
    refs.iter().map(|r| {
        let data = loaded.get(&r.file).map(|d| &**d);
        (r, label(r, data.is_none()), data)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn input(n: u32, t: &str, b: &[u8]) -> ImageInput {
        ImageInput { number: n, media_type: t.into(), data: base64::engine::general_purpose::STANDARD.encode(b) }
    }

    #[test]
    fn an_image_is_held_to_the_format_it_names_and_the_limits() {
        assert_eq!(decode(&[input(1, "image/png", PNG)]).unwrap()[0].media_type, "image/png");
        assert_eq!(decode(&[input(1, "image/jpeg", PNG)]).unwrap_err().code, "bad_image");
        assert_eq!(decode(&[input(1, "image/png", b"hello")]).unwrap_err().code, "bad_image");
        assert_eq!(decode(&[ImageInput { number: 1, media_type: "image/png".into(), data: "!!".into() }]).unwrap_err().code, "bad_image");
        let big = [PNG, &vec![0; MAX_BYTES]].concat();
        assert_eq!(decode(&[input(1, "image/png", &big)]).unwrap_err().code, "bad_image");
        let many: Vec<ImageInput> = (0..=MAX_IMAGES as u32).map(|n| input(n, "image/png", PNG)).collect();
        assert_eq!(decode(&many).unwrap_err().code, "bad_image");
    }

    #[test]
    fn kept_images_read_back_and_a_log_cannot_point_out_of_the_directory() {
        let root = std::env::temp_dir().join(format!("krowk-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.as_path();
        let refs = save(dir,&decode(&[input(1, "image/png", PNG), input(1, "image/png", PNG)]).unwrap()).unwrap();
        assert_ne!(refs[0].file, refs[1].file, "two images numbered alike are two files");
        let mut loaded = Loaded::new();
        load(dir, &refs, &mut loaded);
        assert_eq!(loaded.len(), 2);
        for file in ["../events.jsonl", "/etc/passwd", ".hidden", ""] {
            assert!(path(dir, &ImageRef { number: 1, media_type: "image/png".into(), file: file.into() }).is_none(), "{file}");
        }
        let gone = ImageRef { number: 2, media_type: "image/png".into(), file: "2-x.png".into() };
        load(dir, [&gone], &mut loaded);
        assert!(!loaded.contains_key("2-x.png"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
