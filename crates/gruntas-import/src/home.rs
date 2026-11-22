//! Paths inside the home directory, and nowhere else. A transcript path is
//! caller-controlled — a harness writes it, a symlink can redirect it — so a
//! path is resolved through its symlinks and must still be under the real
//! home before it is opened, and the open itself refuses to follow one.

use crate::{Env, ImportError};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// A transcript is read whole into memory at most this large.
pub const DEFAULT_MAX_BYTES: u64 = 64 << 20;

/// $HOME, cleaned; "" when unset.
pub fn home_dir(env: Env) -> String {
    let home = env("HOME");
    if home.is_empty() { home } else { clean(Path::new(&home)).display().to_string() }
}

/// Lexical cleaning, as Go's filepath.Clean: `.` dropped, `..` folded.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

fn under(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir)
}

/// `rel` (relative to home, or absolute) as a real path inside the real home.
pub fn home_path(env: Env, rel: &str) -> Result<PathBuf, ImportError> {
    let home = home_dir(env);
    if home.is_empty() {
        return Err(ImportError::NoHome("no home directory in environment".into()));
    }
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(ImportError::NoHome(format!("no home directory in environment: home {:?} is not absolute", home.display().to_string())));
    }
    if home.parent().is_none() {
        return Err(ImportError::NoHome(format!("no home directory in environment: home {:?} is a filesystem root", home.display().to_string())));
    }
    let path = clean(&if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { home.join(rel) });
    if !under(&home, &path) {
        return Err(ImportError::OutsideHome(format!("path is outside the home directory: {}", path.display())));
    }
    let real_home = home.canonicalize().map_err(|e| ImportError::Other(format!("resolve home: {e}")))?;
    let real = resolve_existing(&path)?;
    if !under(&real_home, &real) {
        return Err(ImportError::EscapingSymlink(format!(
            "symlink does not resolve inside the home directory: {} resolves to {}",
            path.display(),
            real.display()
        )));
    }
    Ok(real)
}

/// The path with every existing prefix resolved through its symlinks, and
/// the missing tail kept — a dangling symlink on the way is refused.
fn resolve_existing(path: &Path) -> Result<PathBuf, ImportError> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(resolved) => {
                let mut out = resolved;
                for m in missing.iter().rev() {
                    out.push(m);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if cur.symlink_metadata().is_ok() {
                    return Err(ImportError::EscapingSymlink(format!("symlink does not resolve inside the home directory: {} does not resolve", cur.display())));
                }
                let Some(parent) = cur.parent().map(Path::to_path_buf) else {
                    return Err(ImportError::Other(format!("resolve {}: file does not exist", path.display())));
                };
                if let Some(name) = cur.file_name() {
                    missing.push(name.to_os_string());
                }
                if parent == cur {
                    return Err(ImportError::Other(format!("resolve {}: file does not exist", path.display())));
                }
                cur = parent;
            }
            Err(e) => return Err(ImportError::Other(format!("resolve {}: {e}", cur.display()))),
        }
    }
}

/// Opens a regular file inside home without following a final symlink, and
/// refuses one over `max_bytes` (0 is the default cap).
pub fn open_home(env: Env, rel: &str, max_bytes: u64) -> Result<(std::fs::File, PathBuf), ImportError> {
    let max = if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes };
    let path = home_path(env, rel)?;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc_flags());
    let file = o.open(&path).map_err(|e| {
        if e.raw_os_error() == Some(ELOOP) {
            ImportError::EscapingSymlink(format!("{}: symlink does not resolve inside the home directory", path.display()))
        } else if e.raw_os_error() == Some(ENXIO) {
            ImportError::NotRegularFile(format!("{}: not a regular file", path.display()))
        } else {
            ImportError::Other(format!("open {}: {e}", path.display()))
        }
    })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(ImportError::NotRegularFile(format!("{}: not a regular file", path.display())));
    }
    if meta.len() > max {
        return Err(ImportError::TooLarge(format!("{}: file is larger than the read limit ({} > {max} bytes)", path.display(), meta.len())));
    }
    Ok((file, path))
}

/// A whole file inside home, capped.
pub fn read_home(env: Env, rel: &str, max_bytes: u64) -> Result<Vec<u8>, ImportError> {
    let max = if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes };
    let (file, path) = open_home(env, rel, max)?;
    let mut data = Vec::new();
    file.take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(ImportError::TooLarge(format!("{}: file is larger than the read limit (over {max} bytes)", path.display())));
    }
    Ok(data)
}

// O_NOFOLLOW so a final symlink is not followed; O_NONBLOCK so a FIFO put
// where a transcript belongs cannot hang the import. From libc, because the
// values differ by architecture: O_NOFOLLOW is 0o400000 on x86_64 Linux but
// 0o100000 on aarch64, where 0o400000 is O_LARGEFILE.
#[cfg(unix)]
fn libc_flags() -> i32 {
    libc::O_NOFOLLOW | libc::O_NONBLOCK
}
#[cfg(unix)]
const ELOOP: i32 = libc::ELOOP;
#[cfg(unix)]
const ENXIO: i32 = libc::ENXIO;
#[cfg(not(unix))]
const ELOOP: i32 = -1;
#[cfg(not(unix))]
const ENXIO: i32 = -1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_real_home() {
        let dir = std::env::temp_dir().join(format!("gruntas-import-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home/.claude")).unwrap();
        std::fs::write(dir.join("home/.claude/a.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("secret"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("home/.claude/evil.jsonl")).unwrap();
        let home = dir.join("home").display().to_string();
        let env = move |k: &str| if k == "HOME" { home.clone() } else { String::new() };
        assert!(open_home(&env, ".claude/a.jsonl", 0).is_ok());
        assert!(matches!(home_path(&env, "../secret"), Err(ImportError::OutsideHome(_))));
        #[cfg(unix)]
        assert!(matches!(open_home(&env, ".claude/evil.jsonl", 0), Err(ImportError::EscapingSymlink(_))));
        assert!(matches!(open_home(&env, ".claude/a.jsonl", 1), Err(ImportError::TooLarge(_))));
        assert!(matches!(home_path(&|_: &str| String::new(), "x"), Err(ImportError::NoHome(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
