//! The images a person attaches to a prompt. A client sends their bytes
//! with the prompt (`ImageInput`), so a host on another machine gets them
//! too; the host checks them, keeps them in the session's `images/`
//! directory beside its log, and logs only a reference (`ImageRef`). A
//! native turn reads them back for its model calls, a backend is handed
//! the file.
//!
//! A client is expected to have made them small enough for any provider
//! already (the TUI does, as it pastes them); the host holds them to the
//! limits below and never resizes. Every image in a session's history
//! goes with every model call, so a call sends the newest that fit
//! (`SENT_IMAGES`, `SENT_BYTES`) and names the rest, and a model that
//! reads no images is sent their names alone: a session never comes to a
//! request its provider refuses.

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

/// The longest side an image may have: what the providers read.
pub const MAX_SIDE: u32 = 8000;

/// The most images one prompt or steer may carry.
pub const MAX_IMAGES: usize = 20;

/// The most images a model call sends, newest first: past 20, Anthropic
/// holds each to 2000 pixels a side.
pub const SENT_IMAGES: usize = 20;

/// The most base64 a model call sends, all images together: well inside the
/// smallest request a provider takes (Anthropic's 32 MB), the text with it.
pub const SENT_BYTES: usize = 20 * 1024 * 1024;

const GONE: &str = "the file is gone";
const NOT_READ: &str = "not sent, as this model reads no images";
const EARLIER: &str = "not sent again, as it was shown earlier";

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

/// Width and height, from the header alone.
pub fn dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u32::from(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?])));
    let le16 = |i: usize| Some(u32::from(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?])));
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    match sniff(b)? {
        "image/png" => Some((be32(16)?, be32(20)?)),
        "image/gif" => Some((le16(6)?, le16(8)?)),
        "image/webp" => match b.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => {
                let three = |i: usize| Some((u32::from(*b.get(i)?) | u32::from(*b.get(i + 1)?) << 8 | u32::from(*b.get(i + 2)?) << 16) + 1);
                Some((three(24)?, three(27)?))
            }
            _ => None,
        },
        // A JPEG's size is in its first start-of-frame marker, after
        // whatever EXIF and tables come first.
        _ => {
            let mut i = 2;
            loop {
                if *b.get(i)? != 0xff {
                    return None;
                }
                let marker = *b.get(i + 1)?;
                match marker {
                    0xff => i += 1,
                    0x01 | 0xd0..=0xd8 => i += 2,
                    0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => return Some((be16(i + 7)?, be16(i + 5)?)),
                    _ => i += 2 + be16(i + 2)? as usize,
                }
            }
        }
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
    // What one model call sends of them all: a prompt always fits whole.
    let total: usize = inputs.iter().map(|i| i.data.len()).sum();
    if total > SENT_BYTES {
        return Err(refused(format!("the images come to {} MB, and a prompt's may come to at most {} MB", total / (1024 * 1024), SENT_BYTES / (1024 * 1024))));
    }
    let too_big = |n: u32, bytes: usize| refused(format!("[Image #{n}] is {} KB, and an image may be at most {} KB", bytes / 1024, MAX_BYTES / 1024));
    inputs
        .iter()
        .map(|i| {
            let n = i.number;
            // Measured before it is decoded: base64 is four bytes for three.
            if i.data.len() / 4 * 3 > MAX_BYTES + 3 {
                return Err(too_big(n, i.data.len() / 4 * 3));
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(i.data.as_bytes()).map_err(|_| refused(format!("[Image #{n}] is not base64")))?;
            if bytes.len() > MAX_BYTES {
                return Err(too_big(n, bytes.len()));
            }
            let t = match sniff(&bytes) {
                Some(t) if t == i.media_type => t,
                Some(t) => return Err(refused(format!("[Image #{n}] says it is {} but is {t}", i.media_type))),
                None => return Err(refused(format!("[Image #{n}] is not a PNG, JPEG, GIF or WebP image"))),
            };
            match dimensions(&bytes) {
                Some((w, h)) if w.max(h) <= MAX_SIDE && w > 0 && h > 0 => Ok(Decoded { number: n, media_type: t, bytes: bytes.into() }),
                Some((w, h)) => Err(refused(format!("[Image #{n}] is {w}x{h}, and an image may be at most {MAX_SIDE} pixels a side"))),
                None => Err(refused(format!("[Image #{n}] has a header that could not be read"))),
            }
        })
        .collect()
}

/// FNV-1a: a name for the bytes, so the same image kept twice (a prompt
/// rolled over to another instance, steering sent again) is one file.
fn digest(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ u64::from(*x)).wrapping_mul(0x0100_0000_01b3))
}

/// Keeps the images in `session_dir`'s `images/`, private to the person,
/// each named by its number and its bytes: written whole under another name
/// first, so a file of that name is always the whole image.
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
            let file = format!("{}-{:016x}.{}", i.number, digest(&i.bytes), extension(i.media_type));
            let part = dir.join(format!(".{file}.{}", krowk_store::new_id()));
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let written = o.open(&part).and_then(|mut f| std::io::Write::write_all(&mut f, &i.bytes)).and_then(|()| std::fs::rename(&part, dir.join(&file)));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&part);
                return Err(failed(e));
            }
            Ok(ImageRef { number: i.number, media_type: i.media_type.to_string(), file })
        })
        .collect()
}

/// Where a logged image's file is, or none for a name that is not a plain
/// file name: a log is read, never trusted to point elsewhere.
pub fn path(session_dir: &Path, r: &ImageRef) -> Option<PathBuf> {
    let plain = !r.file.is_empty() && !r.file.starts_with('.') && !r.file.contains(['/', '\\', ':', '\0']);
    plain.then(|| session_dir.join(DIR).join(&r.file))
}

/// The images a model call sends, as base64 by file name, and why each
/// other one it names is not sent: read once a turn and kept, so a turn of
/// many calls reads each file once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    data: HashMap<String, Arc<str>>,
    held: HashMap<String, &'static str>,
}

impl Loaded {
    pub fn insert(&mut self, file: String, data: Arc<str>) {
        self.held.remove(&file);
        self.data.insert(file, data);
    }

    pub fn get(&self, file: &str) -> Option<&str> {
        self.data.get(file).map(|d| &**d)
    }
}

fn read(session_dir: &Path, r: &ImageRef) -> Option<Arc<str>> {
    let bytes = std::fs::read(path(session_dir, r)?).ok()?;
    (sniff(&bytes) == Some(r.media_type.as_str())).then(|| base64::engine::general_purpose::STANDARD.encode(bytes).into())
}

/// Chooses what of `refs` (the history's images, oldest first) a call
/// sends: the newest, up to `SENT_IMAGES` and `SENT_BYTES` together, read
/// unless `loaded` holds them already; none when the model `reads` none.
pub fn load(session_dir: &Path, refs: &[&ImageRef], reads: bool, loaded: &mut Loaded) {
    let mut next = Loaded::default();
    let (mut count, mut bytes) = (0usize, 0usize);
    for r in refs.iter().rev() {
        if next.data.contains_key(&r.file) || next.held.contains_key(&r.file) {
            continue;
        }
        if !reads {
            next.held.insert(r.file.clone(), NOT_READ);
            continue;
        }
        // Measured before it is read: one that would not fit is not read.
        let size = match loaded.data.get(&r.file) {
            Some(d) => d.len(),
            None => match path(session_dir, r).and_then(|p| std::fs::metadata(p).ok()) {
                Some(m) => (m.len() as usize).div_ceil(3) * 4,
                None => continue,
            },
        };
        if count >= SENT_IMAGES || bytes + size > SENT_BYTES {
            next.held.insert(r.file.clone(), EARLIER);
            continue;
        }
        if let Some(data) = loaded.data.get(&r.file).cloned().or_else(|| read(session_dir, r)) {
            count += 1;
            bytes += data.len();
            next.data.insert(r.file.clone(), data);
        }
    }
    *loaded = next;
}

/// The label a provider is sent before an image, so the model can tell
/// which `[Image #N]` the text means, and in place of one not sent, why.
pub fn label(r: &ImageRef, why_not: Option<&str>) -> String {
    match why_not {
        Some(why) => format!("[Image #{}: {why}]", r.number),
        None => format!("[Image #{}]", r.number),
    }
}

/// A label for an image whose file is gone.
pub fn gone(r: &ImageRef) -> String {
    label(r, Some(GONE))
}

/// What a native provider is sent for each image an item carries: the
/// label, and the base64 when the image is sent. `shown` is what the
/// request has sent so far: an image two items name (steering sent twice)
/// is sent once, and named after that.
pub fn sent<'a>(refs: &'a [ImageRef], loaded: &'a Loaded, shown: &mut std::collections::HashSet<&'a str>) -> Vec<(&'a ImageRef, String, Option<&'a str>)> {
    refs.iter()
        .map(|r| match loaded.get(&r.file) {
            Some(_) if !shown.insert(&r.file) => (r, label(r, Some(EARLIER)), None),
            Some(data) => (r, label(r, None), Some(data)),
            None => (r, label(r, Some(loaded.held.get(&r.file).copied().unwrap_or(GONE))), None),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

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
        let heavy = [PNG, &vec![0; MAX_BYTES - PNG.len()]].concat();
        let six: Vec<ImageInput> = (0..6).map(|n| input(n, "image/png", &heavy)).collect();
        assert!(decode(&six).unwrap_err().message.contains("at most 20 MB"), "a prompt always fits one call whole");
    }

    #[test]
    fn a_size_is_read_from_the_header_and_held_to_the_providers_limit() {
        assert_eq!(dimensions(PNG), Some((1, 1)));
        assert_eq!(dimensions(b"GIF89a\x20\x03\x58\x02"), Some((800, 600)));
        let jpeg = [&b"\xff\xd8\xff\xe0\0\x04ab"[..], b"\xff\xdb\0\x03x", b"\xff\xc0\0\x11\x08\x02\x58\x03\x20"].concat();
        assert_eq!(dimensions(&jpeg), Some((800, 600)), "past the tables to the frame");
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0; 8]);
        webp.extend([0x1f, 0x03, 0, 0x57, 0x02, 0]);
        assert_eq!(dimensions(&webp), Some((800, 600)));
        let wide = [&PNG[..16], &9000u32.to_be_bytes(), &1u32.to_be_bytes()].concat();
        assert!(decode(&[input(1, "image/png", &wide)]).unwrap_err().message.contains("9000x1"));
        let colon = ImageRef { number: 1, media_type: "image/png".into(), file: "C:x.png".into() };
        assert!(path(Path::new("/s"), &colon).is_none());
    }

    #[test]
    fn kept_images_read_back_and_a_log_cannot_point_out_of_the_directory() {
        let root = std::env::temp_dir().join(format!("krowk-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.as_path();
        let other = [PNG, b"x"].concat();
        let refs = save(dir, &decode(&[input(1, "image/png", PNG), input(1, "image/png", &other)]).unwrap()).unwrap();
        assert_ne!(refs[0].file, refs[1].file, "two images numbered alike are two files");
        let again = save(dir, &decode(&[input(1, "image/png", PNG)]).unwrap()).unwrap();
        assert_eq!(again[0].file, refs[0].file, "the same image kept again is the same file");
        assert_eq!(std::fs::read_dir(dir.join(DIR)).unwrap().count(), 2, "and nothing half-written is left");
        let all: Vec<&ImageRef> = refs.iter().collect();
        let mut loaded = Loaded::default();
        load(dir, &all, true, &mut loaded);
        assert!(sent(&refs, &loaded, &mut Default::default()).into_iter().all(|(_, label, data)| data.is_some() && !label.contains(':')));
        load(dir, &all, false, &mut loaded);
        assert!(sent(&refs, &loaded, &mut Default::default()).into_iter().all(|(_, label, data)| data.is_none() && label.contains("reads no images")), "a model that reads none is sent their names");
        for file in ["../events.jsonl", "/etc/passwd", ".hidden", ""] {
            assert!(path(dir, &ImageRef { number: 1, media_type: "image/png".into(), file: file.into() }).is_none(), "{file}");
        }
        let gone = ImageRef { number: 2, media_type: "image/png".into(), file: "2-x.png".into() };
        load(dir, &[&gone], true, &mut loaded);
        assert_eq!(sent(std::slice::from_ref(&gone), &loaded, &mut Default::default()).remove(0).1, "[Image #2: the file is gone]");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_call_sends_the_newest_images_that_fit_and_names_the_rest() {
        let root = std::env::temp_dir().join(format!("krowk-images-sent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.as_path();
        let inputs: Vec<ImageInput> = (1..=SENT_IMAGES as u32 + 2).map(|n| input(n, "image/png", &[PNG, &n.to_be_bytes()].concat())).collect();
        let refs = save(dir, &decode(&inputs[..MAX_IMAGES]).unwrap()).unwrap().into_iter().chain(save(dir, &decode(&inputs[MAX_IMAGES..]).unwrap()).unwrap()).collect::<Vec<_>>();
        let mut loaded = Loaded::default();
        load(dir, &refs.iter().collect::<Vec<_>>(), true, &mut loaded);
        let shown: Vec<bool> = sent(&refs, &loaded, &mut Default::default()).into_iter().map(|(_, _, d)| d.is_some()).collect();
        assert_eq!(shown.iter().filter(|s| **s).count(), SENT_IMAGES);
        assert!(!shown[0] && !shown[1] && shown[2], "the oldest two are named, not sent");
        assert!(sent(&refs[..1], &loaded, &mut Default::default()).remove(0).1.contains("shown earlier"));
        let twice = [refs[5].clone(), refs[5].clone()];
        let mut shown = Default::default();
        let first = sent(&twice[..1], &loaded, &mut shown);
        let second = sent(&twice[1..], &loaded, &mut shown);
        assert!(first[0].2.is_some() && second[0].2.is_none(), "an image two items name is sent once");
        let _ = std::fs::remove_dir_all(&root);
    }
}
