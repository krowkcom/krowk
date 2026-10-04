//! Images into the prompt. A screenshot on the clipboard comes in with
//! Ctrl-V (Alt-V where the terminal keeps Ctrl-V for its own paste, as
//! Windows Terminal does), and so does a paste the terminal sends empty,
//! which is what some send for a clipboard holding only an image. A file
//! dragged onto the terminal, or a path pasted, comes in as its image when
//! every path in the paste is one. Each becomes `[Image #N]` in the prompt.
//!
//! The clipboard is read with the desktop's own commands, as `clipboard`
//! writes it: `osascript` on macOS, `wl-paste` or `xclip` on Linux,
//! PowerShell on Windows and under WSL. Never on the UI's thread.
//!
//! An image is made small enough for any provider as it is pasted, so what
//! the prompt holds is what is sent: at most `MAX_SIDE` pixels a side and
//! `MAX_BYTES`, kept as it came when it already is, else resized and
//! written as PNG, or as JPEG when a PNG is too big. PNG, JPEG, GIF and
//! WebP are read; a clipboard is asked for PNG, which every desktop
//! converts to. A file is read the moment it is dropped: macOS deletes a
//! screenshot dragged from its thumbnail soon after.

use base64::Engine as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest side an image is sent at: what the providers read at full
/// detail, and well past what a screenshot needs to be legible.
pub const MAX_SIDE: u32 = 2000;

/// The most bytes an image is sent as: the host's limit.
pub const MAX_BYTES: usize = krowk_harness::images::MAX_BYTES;

/// How long a clipboard command may take: a big screenshot through
/// `osascript` takes a while, a hung clipboard owner forever.
const READ_WAIT: Duration = Duration::from_secs(5);

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

/// What the clipboard holds, as far as a prompt cares.
#[derive(Debug, PartialEq)]
pub enum Clip {
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
    Text(String),
    Empty,
}

/// What a paste brought: its images, or the text to type instead, and why
/// any image was left out.
#[derive(Debug, Default)]
pub struct Pasted {
    pub images: Vec<Image>,
    pub text: Option<String>,
    pub problem: Option<String>,
}

/// Ctrl-V: the clipboard, read and made ready. Blocking.
pub fn from_clipboard() -> Pasted {
    match read_clipboard() {
        Ok(Clip::Image(bytes)) => one(normalise(&bytes)),
        Ok(Clip::Files(paths)) => from_files(&paths, None),
        Ok(Clip::Text(t)) => match dropped(&t) {
            Some(paths) => from_files(&paths, Some(t)),
            None => Pasted { text: Some(t), ..Pasted::default() },
        },
        Ok(Clip::Empty) => Pasted { problem: Some("the clipboard is empty".into()), ..Pasted::default() },
        Err(e) => Pasted { problem: Some(e), ..Pasted::default() },
    }
}

/// The images at `paths`, read now. `text`, when given, is what goes into
/// the prompt instead should any of them not be an image after all.
pub fn from_files(paths: &[PathBuf], text: Option<String>) -> Pasted {
    let mut images = Vec::new();
    for p in paths {
        let read = std::fs::read(p).map_err(|e| format!("{} could not be read: {e}", p.display()));
        match read.and_then(|b| normalise(&b).map_err(|e| format!("{}: {e}", p.display()))) {
            Ok(i) => images.push(i),
            Err(e) => return Pasted { text, problem: Some(e), images: Vec::new() },
        }
    }
    Pasted { images, ..Pasted::default() }
}

fn one(r: Result<Image, String>) -> Pasted {
    match r {
        Ok(i) => Pasted { images: vec![i], ..Pasted::default() },
        Err(e) => Pasted { problem: Some(e), ..Pasted::default() },
    }
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
    Err(format!("the image is too big to send, even made smaller (over {} MB)", MAX_BYTES / (1024 * 1024)))
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
fn tokens(text: &str) -> Option<Vec<String>> {
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
fn to_path(token: &str) -> Option<PathBuf> {
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

fn wsl() -> bool {
    cfg!(target_os = "linux") && (std::env::var_os("WSL_DISTRO_NAME").is_some() || std::fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft")))
}

/// The clipboard: an image, files copied in a file manager, or text.
pub fn read_clipboard() -> Result<Clip, String> {
    if cfg!(target_os = "macos") {
        macos()
    } else if cfg!(windows) || wsl() {
        windows()
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        unix_tool(&Tool::WL_PASTE).or_else(|e| if std::env::var_os("DISPLAY").is_some() { unix_tool(&Tool::XCLIP) } else { Err(e) })
    } else if std::env::var_os("DISPLAY").is_some() {
        unix_tool(&Tool::XCLIP)
    } else {
        Err("there is no clipboard to read here — over SSH, drag the file in or paste its path".into())
    }
}

/// One of the desktop's clipboard commands, asked for a type by name.
struct Tool {
    program: &'static str,
    list: &'static [&'static str],
    get: &'static [&'static str],
    package: &'static str,
}

impl Tool {
    const WL_PASTE: Tool = Tool { program: "wl-paste", list: &["--list-types"], get: &["--no-newline", "--type"], package: "wl-clipboard" };
    const XCLIP: Tool = Tool { program: "xclip", list: &["-selection", "clipboard", "-t", "TARGETS", "-o"], get: &["-selection", "clipboard", "-o", "-t"], package: "xclip" };
}

fn unix_tool(t: &Tool) -> Result<Clip, String> {
    let types = run(t.program, t.list).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("install {} to paste images", t.package) } else { format!("{} failed: {e}", t.program) })?;
    let types: Vec<String> = String::from_utf8_lossy(&types).lines().map(|l| l.trim().to_string()).collect();
    let get = |ty: &str| run(t.program, &[t.get, &[ty]].concat()).map_err(|e| format!("{} failed: {e}", t.program));
    if types.iter().any(|ty| ty == "text/uri-list") {
        let files: Vec<PathBuf> = String::from_utf8_lossy(&get("text/uri-list")?).lines().filter_map(|l| to_path(l.trim())).collect();
        if !files.is_empty() {
            return Ok(Clip::Files(files));
        }
    }
    let image = ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp", "image/tiff"].into_iter().find(|w| types.iter().any(|ty| ty == w));
    if let Some(ty) = image {
        return Ok(Clip::Image(get(ty)?));
    }
    let text = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"].into_iter().find(|w| types.iter().any(|ty| ty == w));
    match text {
        Some(ty) => Ok(Clip::Text(String::from_utf8_lossy(&get(ty)?).into_owned())),
        None => Ok(Clip::Empty),
    }
}

/// One `osascript`: a copied file's path, else the image as PNG (which
/// macOS converts a TIFF or a JPEG on the clipboard to), printed as hex.
fn macos() -> Result<Clip, String> {
    const SCRIPT: &str = "try\nreturn \"file:\" & (POSIX path of (the clipboard as «class furl»))\nend try\ntry\nreturn the clipboard as «class PNGf»\nend try\ntry\nreturn the clipboard as «class TIFF»\nend try\nreturn \"\"";
    let out = run("osascript", &["-e", SCRIPT]).map_err(|e| format!("osascript failed: {e}"))?;
    let out = String::from_utf8_lossy(&out);
    let out = out.trim();
    if let Some(path) = out.strip_prefix("file:") {
        return Ok(Clip::Files(vec![PathBuf::from(path)]));
    }
    if let Some(hex) = out.strip_prefix("«data PNGf").or_else(|| out.strip_prefix("«data TIFF")) {
        return unhex(hex.trim_end_matches('»')).map(Clip::Image).ok_or_else(|| "the clipboard's image could not be read".to_string());
    }
    match run("pbpaste", &[]) {
        Ok(t) if !t.is_empty() => Ok(Clip::Text(String::from_utf8_lossy(&t).into_owned())),
        _ => Ok(Clip::Empty),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    b.len().is_multiple_of(2).then(|| b.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()).flatten()
}

/// PowerShell, on Windows and from WSL: files copied in Explorer, else the
/// image as PNG, base64; else the text.
fn windows() -> Result<Clip, String> {
    const SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $c=[System.Windows.Forms.Clipboard]; $f=$c::GetFileDropList(); if ($f.Count -gt 0) { 'files:' + ($f -join '|'); exit }; $i=$c::GetImage(); if ($i) { $m=New-Object System.IO.MemoryStream; $i.Save($m,[System.Drawing.Imaging.ImageFormat]::Png); 'png:' + [Convert]::ToBase64String($m.ToArray()); exit }; if ($c::ContainsText()) { 'text:' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($c::GetText())) }";
    let out = run("powershell.exe", &["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT]).map_err(|e| format!("PowerShell failed: {e}"))?;
    let out = String::from_utf8_lossy(&out);
    let out = out.trim();
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|_| "the clipboard could not be read".to_string());
    if let Some(files) = out.strip_prefix("files:") {
        return Ok(Clip::Files(files.split('|').filter_map(|f| to_path(f.trim())).collect()));
    }
    if let Some(png) = out.strip_prefix("png:") {
        return b64(png).map(Clip::Image);
    }
    if let Some(text) = out.strip_prefix("text:") {
        return b64(text).map(|t| Clip::Text(String::from_utf8_lossy(&t).into_owned()));
    }
    Ok(Clip::Empty)
}

/// A command's output, given `READ_WAIT` at most.
fn run(program: &str, args: &[&str]) -> std::io::Result<Vec<u8>> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + READ_WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            let out = reader.join().unwrap_or_default();
            // A clipboard with nothing of the type asked for exits non-zero.
            return Ok(if status.success() { out } else { Vec::new() });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "it did not answer in time"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_small_image_is_kept_as_it_came_and_a_big_one_is_made_to_fit() {
        let small = png(40, 30);
        let i = normalise(&small).unwrap();
        assert_eq!((i.media_type, &*i.bytes, i.width, i.height), ("image/png", &small[..], 40, 30));
        let big = normalise(&png(4000, 1000)).unwrap();
        assert_eq!((big.width, big.height), (2000, 500), "the long side to MAX_SIDE, the aspect kept");
        assert!(big.bytes.len() <= MAX_BYTES);
        let mut bmp = b"BM".to_vec();
        bmp.extend([0; 60]);
        assert!(normalise(&bmp).unwrap_err().contains("PNG, JPEG, GIF or WebP"), "a format krowk does not read says so");
        assert!(normalise(b"not an image at all").is_err());
    }

    #[test]
    fn a_photo_too_big_as_png_is_sent_as_jpeg() {
        // Noise: no PNG compresses it under the limit.
        let mut seed = 1u32;
        let noise = image::RgbImage::from_fn(1900, 1900, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        noise.write_to(&mut out, image::ImageFormat::Png).unwrap();
        let i = normalise(&out.into_inner()).unwrap();
        assert_eq!(i.media_type, "image/jpeg");
        assert!(i.bytes.len() <= MAX_BYTES);
    }

    #[test]
    fn a_dropped_file_is_read_however_the_terminal_quoted_it() {
        let dir = std::env::temp_dir().join(format!("krowk-paste-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("Screenshot 2026-10-04 at 10.00.png");
        std::fs::write(&shot, png(4, 4)).unwrap();
        let other = dir.join("b.png");
        std::fs::write(&other, png(4, 4)).unwrap();
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "hello").unwrap();
        let s = shot.display().to_string();
        let o = other.display().to_string();
        for pasted in [format!("'{s}'"), format!("\"{s}\""), s.replace(' ', "\\ "), format!("file://{}", s.replace(' ', "%20")), format!("  '{s}'\n")] {
            assert_eq!(dropped(&pasted), Some(vec![shot.clone()]), "{pasted}");
        }
        assert_eq!(dropped(&format!("'{s}' {o}")), Some(vec![shot.clone(), other.clone()]), "two files dropped at once");
        assert_eq!(dropped(&format!("'{s}' {}", notes.display())), None, "a file that is not an image keeps the paste text");
        assert_eq!(dropped(&format!("look at '{s}'")), None, "a sentence stays a sentence");
        assert_eq!(dropped(&dir.join("gone.png").display().to_string()), None);
        assert_eq!(dropped(&format!("'{s}")), None, "an open quote");
        #[cfg(unix)]
        {
            let fifo = dir.join("pipe.png");
            assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            assert_eq!(dropped(&fifo.display().to_string()), None, "a FIFO is never opened, so nothing waits on a writer");
        }
        let p = from_files(std::slice::from_ref(&shot), None);
        assert_eq!(p.images.len(), 1);
        let p = from_files(std::slice::from_ref(&notes), Some("x".into()));
        assert!(p.images.is_empty() && p.problem.is_some() && p.text.as_deref() == Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_and_windows_paths_read_as_they_are_written() {
        assert_eq!(unhex("89504e47"), Some(vec![0x89, 0x50, 0x4e, 0x47]));
        assert_eq!(unhex("8"), None);
        assert_eq!(tokens("C:\\Users\\a b.png").unwrap(), ["C:\\Users\\a", "b.png"]);
        assert_eq!(tokens("\"C:\\Users\\a b.png\"").unwrap(), ["C:\\Users\\a b.png"]);
    }
}
