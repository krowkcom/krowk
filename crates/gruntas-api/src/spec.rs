//! Measuring a file before it is declared: size, digest, and the content
//! type that is signed into the upload URL.

use crate::error::{fail, Error};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

/// A file about to be uploaded, measured and digested so the registry can sign
/// an upload URL only these exact bytes fit. `path` is local and never sent:
/// the registry is told the basename, not where the file sat.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Spec {
    #[serde(skip)]
    pub path: String,
    pub filename: String,
    pub content_type: String,
    pub byte_size: i64,
    pub checksum: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub run: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Go's wording for an operating-system failure, `open <path>: permission
/// denied`, rebuilt from the errno so a message reads the same from either
/// build.
pub fn go_os_error(op: &str, path: &str, err: &std::io::Error) -> String {
    let text = match err.raw_os_error() {
        Some(code) => {
            let s = std::io::Error::from_raw_os_error(code).to_string();
            let s = s.split(" (os error").next().unwrap_or(&s).to_string();
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
                None => s,
            }
        }
        None => err.to_string(),
    };
    format!("{op} {path}: {text}")
}

/// Measures and digests a file. The digest is read up front because it is
/// signed into the upload URL, which is what lets storage refuse corrupted
/// bytes at the edge.
pub fn inspect(path: &str) -> Result<Spec, Error> {
    let unreadable = || fail("file_unreadable", format!("cannot read `{path}` — paths resolve from the current directory"));
    let meta = std::fs::metadata(path).map_err(|_| unreadable())?;
    if !meta.is_file() {
        return Err(unreadable());
    }
    // The registry requires a size above zero, and saying so here beats a
    // signature error from storage.
    if meta.len() == 0 {
        return Err(fail("empty_file", format!("`{path}` is empty — there is nothing to upload")));
    }
    let mut file = std::fs::File::open(path)
        .map_err(|e| fail("file_unreadable", format!("cannot read `{path}`: {}", go_os_error("open", path, &e))))?;
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => digest.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(fail(
                    "file_unreadable",
                    format!("could not read all of `{path}`: {}", go_os_error("read", path, &e)),
                ))
            }
        }
    }

    // A .webm/.mkv carrying a video track is video whatever the extension
    // table says, so its head is read back once the digest pass is done.
    let mut head = Vec::new();
    if is_matroska_ext(path) && file.seek(SeekFrom::Start(0)).is_ok() {
        let _ = file.take(MATROSKA_HEAD_LIMIT).read_to_end(&mut head);
    }

    Ok(Spec {
        path: path.to_string(),
        filename: base(path),
        content_type: content_type_for(path, &head),
        byte_size: meta.len() as i64,
        checksum: digest.finalize().iter().map(|b| format!("{b:02x}")).collect(),
        ..Spec::default()
    })
}

/// Go's filepath.Base: the last element, trailing separators ignored.
fn base(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { ".".into() } else { "/".into() };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Go's filepath.Ext: the suffix from the last dot of the last element.
fn ext(path: &str) -> &str {
    let last = path.rsplit('/').next().unwrap_or(path);
    last.rfind('.').map_or("", |i| &last[i..])
}

/// The extension's type, without the charset parameter.
pub fn content_type(path: &str) -> String {
    content_type_for(path, &[])
}

/// The table decides, except that a Matroska head with a video track makes a
/// .webm or .mkv video: the table answers audio/webm for .webm, so an
/// audio-only recording stays audio and a screen recording does not.
fn content_type_for(path: &str, head: &[u8]) -> String {
    let ext = ext(path).to_lowercase();
    let found = CONTENT_TYPES.binary_search_by(|(k, _)| k.cmp(&ext.as_str())).ok().map(|i| CONTENT_TYPES[i].1);
    let t = found.unwrap_or("application/octet-stream");
    if !head.is_empty() && is_matroska_ext(path) && has_matroska_video_track(head) {
        if let Some(rest) = t.strip_prefix("audio/") {
            return format!("video/{rest}");
        }
        if t == "application/octet-stream" {
            return if ext == ".mkv" { "video/x-matroska".into() } else { "video/webm".into() };
        }
    }
    t.to_string()
}

/// How much of the file the video-track sniff reads: tracks sit near the start.
const MATROSKA_HEAD_LIMIT: u64 = 512 << 10;

fn is_matroska_ext(path: &str) -> bool {
    matches!(ext(path).to_lowercase().as_str(), ".webm" | ".mkv")
}

/// An EBML header, then a TrackType of video (1) or a video CodecID. The EBML
/// gate comes first so bytes that merely mention "V_VP9" cannot promote a
/// renamed file.
fn has_matroska_video_track(head: &[u8]) -> bool {
    if head.len() < 4 || head[..4] != [0x1A, 0x45, 0xDF, 0xA3] {
        return false;
    }
    if has_video_track_type(head) {
        return true;
    }
    MATROSKA_VIDEO_CODECS.iter().any(|codec| head.windows(codec.len()).any(|w| w == *codec))
}

/// A TrackType element (0x83) whose one-byte value is 1. The size is read as
/// an EBML varint rather than assumed.
fn has_video_track_type(head: &[u8]) -> bool {
    (0..head.len().saturating_sub(2)).any(|i| {
        head[i] == 0x83 && head[i + 1] & 0x80 != 0 && head[i + 1] & 0x7F == 1 && head[i + 2] == 1
    })
}

const MATROSKA_VIDEO_CODECS: [&[u8]; 15] = [
    b"V_MPEG1", b"V_MPEG2", b"V_MPEG4", b"V_MPEGH", b"V_MPEGI", b"V_MS/VFW", b"V_THEORA", b"V_REAL",
    b"V_QUICKTIME", b"V_VP8", b"V_VP9", b"V_AV1", b"V_VC1", b"V_DIRAC", b"V_PRORES",
];

/// krowk's own extension table, sorted for lookup. The host's mime.types is
/// never read, so one file declares the same from any machine. Every entry is
/// what Go answered on macOS when the table was fixed, except .md/.markdown
/// (text/markdown) and .webm (audio/webm, promoted by a video track).
const CONTENT_TYPES: &[(&str, &str)] = &[
    (".3gp", "video/3gpp"),
    (".7z", "application/x-7z-compressed"),
    (".aac", "audio/x-aac"),
    (".aif", "audio/x-aiff"),
    (".aiff", "audio/x-aiff"),
    (".apng", "image/apng"),
    (".avi", "video/x-msvideo"),
    (".avif", "image/avif"),
    (".bmp", "image/bmp"),
    (".bz2", "application/x-bzip2"),
    (".c", "text/x-c"),
    (".conf", "text/plain"),
    (".cpp", "text/x-c"),
    (".css", "text/css"),
    (".csv", "text/csv"),
    (".doc", "application/msword"),
    (".docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    (".eot", "application/vnd.ms-fontobject"),
    (".epub", "application/epub+zip"),
    (".flac", "audio/x-flac"),
    (".flv", "video/x-flv"),
    (".gif", "image/gif"),
    (".gz", "application/gzip"),
    (".h", "text/x-c"),
    (".htm", "text/html"),
    (".html", "text/html"),
    (".ico", "image/x-icon"),
    (".ics", "text/calendar"),
    (".jar", "application/java-archive"),
    (".java", "text/x-java-source"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".js", "application/javascript"),
    (".json", "application/json"),
    (".log", "text/plain"),
    (".m3u", "audio/x-mpegurl"),
    (".m4a", "audio/mp4a-latm"),
    (".m4v", "video/x-m4v"),
    (".markdown", "text/markdown"),
    (".md", "text/markdown"),
    (".mid", "audio/midi"),
    (".midi", "audio/midi"),
    (".mjs", "text/javascript"),
    (".mkv", "video/x-matroska"),
    (".mov", "video/quicktime"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".mpeg", "video/mpeg"),
    (".odt", "application/vnd.oasis.opendocument.text"),
    (".oga", "audio/ogg"),
    (".ogg", "audio/ogg"),
    (".ogv", "video/ogg"),
    (".opus", "audio/ogg"),
    (".otf", "font/otf"),
    (".pdf", "application/pdf"),
    (".png", "image/png"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (".pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    (".rar", "application/x-rar-compressed"),
    (".rs", "application/rls-services+xml"),
    (".rtf", "application/rtf"),
    (".sh", "application/x-sh"),
    (".sql", "application/x-sql"),
    (".srt", "application/x-subrip"),
    (".svg", "image/svg+xml"),
    (".tar", "application/x-tar"),
    (".text", "text/plain"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".ts", "video/mp2t"),
    (".tsv", "text/tab-separated-values"),
    (".ttf", "font/ttf"),
    (".txt", "text/plain"),
    (".vcf", "text/x-vcard"),
    (".vtt", "text/vtt"),
    (".wasm", "application/wasm"),
    (".wav", "audio/x-wav"),
    (".webm", "audio/webm"),
    (".webp", "image/webp"),
    (".wma", "audio/x-ms-wma"),
    (".wmv", "video/x-ms-wmv"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
    (".xhtml", "application/xhtml+xml"),
    (".xls", "application/vnd.ms-excel"),
    (".xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    (".xml", "application/xml"),
    (".xz", "application/x-xz"),
    (".zip", "application/zip"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_answers_and_is_sorted() {
        assert!(CONTENT_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
        for (name, want) in [
            ("shot.png", "image/png"), ("SHOT.PNG", "image/png"), ("report.md", "text/markdown"),
            ("notes.txt", "text/plain"), ("build.log", "text/plain"), ("page.html", "text/html"),
            ("data.json", "application/json"), ("app.js", "application/javascript"),
            ("feed.xml", "application/xml"), ("clip.mp4", "video/mp4"), ("voice.webm", "audio/webm"),
            ("voice.wav", "audio/x-wav"), ("change.diff", "application/octet-stream"),
            ("Makefile", "application/octet-stream"), ("archive.zip", "application/zip"),
            ("dir.d/file", "application/octet-stream"),
        ] {
            assert_eq!(content_type(name), want, "{name}");
        }
    }

    fn matroska(video: bool, codec: &[u8]) -> Vec<u8> {
        let mut head = vec![0x1A, 0x45, 0xDF, 0xA3];
        head.extend_from_slice(b"webm");
        if video {
            head.extend_from_slice(&[0x83, 0x81, 0x01]);
            head.extend_from_slice(codec);
        } else {
            head.extend_from_slice(&[0x83, 0x81, 0x02]);
            head.extend_from_slice(b"A_OPUS");
        }
        head.extend_from_slice(&[0; 64]);
        head
    }

    #[test]
    fn a_webm_with_a_video_track_is_video_and_one_without_stays_audio() {
        assert_eq!(content_type_for("clip.webm", &matroska(true, b"V_VP9")), "video/webm");
        assert_eq!(content_type_for("CLIP.WEBM", &matroska(true, b"V_VP9")), "video/webm");
        let codec_only = [&[0x1A, 0x45, 0xDF, 0xA3][..], b"webm", b"V_AV1"].concat();
        assert_eq!(content_type_for("codec.webm", &codec_only), "video/webm");
        assert_eq!(content_type_for("voice.webm", &matroska(false, b"")), "audio/webm");
        assert_eq!(content_type_for("notes.webm", b"just text in a webm suit"), "audio/webm");
        assert_eq!(content_type_for("clip.mkv", &matroska(true, b"V_AV1")), "video/x-matroska");
    }

    #[test]
    fn inspect_measures_digests_and_refuses_empty_and_missing() {
        let dir = std::env::temp_dir().join(format!("krowk-spec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let spec = inspect(file.to_str().unwrap()).unwrap();
        assert_eq!((spec.filename.as_str(), spec.byte_size), ("a.txt", 5));
        assert_eq!(spec.checksum, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        std::fs::write(&file, "").unwrap();
        assert_eq!(inspect(file.to_str().unwrap()).unwrap_err().code(), "empty_file");
        assert_eq!(inspect(dir.to_str().unwrap()).unwrap_err().code(), "file_unreadable");
        assert_eq!(inspect("/nonexistent/x").unwrap_err().code(), "file_unreadable");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
