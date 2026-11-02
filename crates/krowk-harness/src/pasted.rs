//! An image made ready to send, and a pasted or dropped path read as one:
//! what the TUI's composer, `krowk -p --image` and the typed line of
//! `krowk sync attach` all use, so there is one way an image is made small
//! enough for any provider. At most `MAX_SIDE` pixels a side and
//! `MAX_BYTES`, kept as it came when it already is, else resized and
//! written as PNG, or as JPEG when a PNG is too big.

use base64::Engine as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The longest side an image is sent at: what the providers read at full
/// detail, and well past what a screenshot needs to be legible.
pub const MAX_SIDE: u32 = 2000;

/// The most bytes an image is sent as: the host's limit.
pub const MAX_BYTES: usize = crate::images::MAX_BYTES;

/// An image ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub media_type: &'static str,
    pub bytes: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

impl Image {
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
}


/// The most a file is read as.
const LOAD_CAP: u64 = 64 << 20;

/// The image files at `paths`, read and made ready, at most `MAX_IMAGES`: a
/// file that is missing or no image refuses them all, naming the file.
pub fn load(paths: &[PathBuf]) -> Result<Vec<Image>, String> {
    if paths.len() > crate::images::MAX_IMAGES {
        return Err(format!("{} images; a prompt takes at most {}", paths.len(), crate::images::MAX_IMAGES));
    }
    paths
        .iter()
        .map(|p| {
            // A regular file only: opening a FIFO or a terminal would wait on it.
            if !std::fs::metadata(p).is_ok_and(|m| m.is_file()) {
                return Err(format!("{} is not a file", p.display()));
            }
            // Never more than a camera's raw file: what is read is held whole.
            let mut bytes = Vec::new();
            let read = std::fs::File::open(p).and_then(|f| f.take(LOAD_CAP + 1).read_to_end(&mut bytes));
            read.map_err(|e| format!("{} could not be read: {e}", p.display()))?;
            if bytes.len() as u64 > LOAD_CAP {
                return Err(format!("{} is over {} MB", p.display(), LOAD_CAP >> 20));
            }
            normalise(&bytes).map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect()
}

/// `images` as a prompt carries them, numbered from 1, and the text that
/// names them: `[Image #1] [Image #2] ` to put before the prompt.
pub fn inputs(images: &[Image]) -> (Vec<crate::protocol::ImageInput>, String) {
    let list = images
        .iter()
        .enumerate()
        .map(|(k, i)| crate::protocol::ImageInput { number: k as u32 + 1, media_type: i.media_type.to_string(), data: i.base64(), sha256: Some(crate::images::sha256(&i.bytes)) })
        .collect::<Vec<_>>();
    let names = (1..=images.len()).map(|n| format!("[Image #{n}] ")).collect();
    (list, names)
}

/// `bytes` as an image any provider takes (the module's note says how).
pub fn normalise(bytes: &[u8]) -> Result<Image, String> {
    use image::ImageFormat as F;
    let unreadable = || "not an image krowk reads (PNG, JPEG, GIF or WebP)".to_string();
    let format = image::guess_format(bytes).map_err(|_| unreadable())?;
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let (width, height) = reader.into_dimensions().map_err(|_| unreadable())?;
    let kept = match format {
        F::Png => Some("image/png"),
        F::Jpeg => Some("image/jpeg"),
        F::Gif => Some("image/gif"),
        F::WebP => Some("image/webp"),
        _ => None,
    };
    if let Some(media_type) = kept
        && width.max(height) <= MAX_SIDE
        && bytes.len() <= MAX_BYTES
    {
        return Ok(Image { media_type, bytes: bytes.into(), width, height });
    }
    let img = image::load_from_memory_with_format(bytes, format).map_err(|e| format!("the image could not be read: {e}"))?;
    let img = if width.max(height) > MAX_SIDE { img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle) } else { img };
    // PNG first, so a screenshot's text stays sharp; JPEG, at falling
    // quality, for what is too big as one (a photo).
    let png = encode(&img, F::Png, 0)?;
    if png.len() <= MAX_BYTES {
        return Ok(Image { media_type: "image/png", bytes: png.into(), width: img.width(), height: img.height() });
    }
    for quality in [85, 70, 55, 40] {
        let jpeg = encode(&img, F::Jpeg, quality)?;
        if jpeg.len() <= MAX_BYTES {
            return Ok(Image { media_type: "image/jpeg", bytes: jpeg.into(), width: img.width(), height: img.height() });
        }
    }
    Err(format!("the image is too big to send, even made smaller (over {} MB)", crate::images::max_mb()))
}

fn encode(img: &image::DynamicImage, format: image::ImageFormat, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    let failed = |e: image::ImageError| format!("the image could not be made smaller: {e}");
    if format == image::ImageFormat::Jpeg {
        let rgb = img.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb).map_err(failed)?;
    } else if img.color().has_alpha() {
        img.to_rgba8().write_to(&mut out, format).map_err(failed)?;
    } else {
        img.to_rgb8().write_to(&mut out, format).map_err(failed)?;
    }
    Ok(out.into_inner())
}

/// A paste that is only paths to images, each one there: what a terminal
/// sends for a file dragged onto it (quoted, or with its spaces escaped, or
/// as a `file://` URL), one or several. None when any part is something
/// else, so a sentence that mentions a path stays a sentence.
pub fn dropped(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > 64 * 1024 {
        return None;
    }
    let mut paths = Vec::new();
    for token in tokens(text)? {
        let p = to_path(&token)?;
        if !looks_like_image(&p) {
            return None;
        }
        paths.push(p);
    }
    (!paths.is_empty()).then_some(paths)
}

/// The words of a pasted line as a shell reads them: quotes, and a
/// backslash before a space, except in a Windows path, whose backslashes
/// are its own.
pub fn tokens(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Some(out);
        }
        let rest: String = chars.clone().skip_while(|c| *c == '"' || *c == '\'').take(3).collect();
        let windows = rest.starts_with("\\\\") || (rest.len() == 3 && rest.as_bytes()[0].is_ascii_alphabetic() && &rest[1..] == ":\\");
        let mut word = String::new();
        let mut quote: Option<char> = None;
        while let Some(&c) = chars.peek() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                // In double quotes a backslash escapes only a quote or
                // itself, so `"C:\Users"` keeps its backslashes.
                (Some('"'), '\\') if !windows && chars.clone().nth(1).is_some_and(|n| n == '"' || n == '\\') => {
                    chars.next();
                    word.push(*chars.peek()?);
                }
                (Some(_), c) => word.push(c),
                (None, '\'' | '"') => quote = Some(c),
                (None, '\\') if !windows => {
                    chars.next();
                    word.push(*chars.peek()?);
                }
                (None, c) if c.is_whitespace() => break,
                (None, c) => word.push(c),
            }
            chars.next();
        }
        if quote.is_some() {
            return None;
        }
        out.push(word);
    }
}

/// A token as a path: absolute, `~/`, a `file://` URL, or a Windows path
/// (under WSL, where its drive is mounted).
pub fn to_path(token: &str) -> Option<PathBuf> {
    if let Some(rest) = token.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        return if path.starts_with('/') { percent_decoded(path).map(PathBuf::from) } else { None };
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let b = token.as_bytes();
    if b.len() > 3 && b[0].is_ascii_alphabetic() && &b[1..3] == b":\\" {
        if cfg!(windows) {
            return Some(PathBuf::from(token));
        }
        return wsl().then(|| PathBuf::from(format!("/mnt/{}/{}", (b[0] as char).to_ascii_lowercase(), token[3..].replace('\\', "/"))));
    }
    (token.starts_with('/') || (cfg!(windows) && token.starts_with("\\\\"))).then(|| PathBuf::from(token))
}

fn percent_decoded(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut b = s.bytes();
    while let Some(c) = b.next() {
        if c == b'%' {
            let hex = [b.next()?, b.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(c);
        }
    }
    String::from_utf8(out).ok()
}

/// A file whose first bytes are an image's: by its bytes, not its name.
fn looks_like_image(p: &Path) -> bool {
    // A regular file only: opening a FIFO or a terminal would wait on it.
    if !std::fs::metadata(p).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = [0u8; 16];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    n > 0 && image::guess_format(&head[..n]).is_ok_and(|f| matches!(f, image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif | image::ImageFormat::WebP | image::ImageFormat::Bmp | image::ImageFormat::Tiff))
}

pub fn wsl() -> bool {
    cfg!(target_os = "linux") && (std::env::var_os("WSL_DISTRO_NAME").is_some() || std::fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft")))
}
