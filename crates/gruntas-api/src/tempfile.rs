//! os.CreateTemp: a fresh file beside the one it will replace, created
//! exclusively with the given mode, named prefix + random + suffix.

use std::path::{Path, PathBuf};

pub fn create(dir: &Path, prefix: &str, suffix: &str, mode: u32) -> std::io::Result<PathBuf> {
    create_file(dir, prefix, suffix, mode).map(|(_, path)| path)
}

/// As `create`, keeping the open file, so nothing reopens it by name.
pub fn create_file(dir: &Path, prefix: &str, suffix: &str, mode: u32) -> std::io::Result<(std::fs::File, PathBuf)> {
    let _ = mode;
    for attempt in 0..10_000u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!("{prefix}{}{suffix}", nanos.wrapping_add(attempt.wrapping_mul(7919)) ^ std::process::id()));
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, mode);
        match open.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not create a temporary file"))
}
