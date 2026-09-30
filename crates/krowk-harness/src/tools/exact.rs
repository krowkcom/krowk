//! File I/O on a path already judged, that follows no symlink the judging
//! did not see (R-PERM-3). Under a sandbox the file tools resolve a path
//! once, to where it really leads, check that, and then open exactly that:
//! a symlink swapped in after the check — by a command running beside the
//! tool — makes the open fail instead of leading the tool into `.git/hooks`
//! or a key outside the workspace.
//!
//! On Linux it is `openat2(RESOLVE_NO_SYMLINKS)` on the checked path, whose
//! every component was a real directory when it was checked, and writes go
//! through the parent directory's descriptor: the temporary file is made
//! with `O_EXCL | O_NOFOLLOW` in it and renamed over the name, which
//! replaces a link swapped in rather than writing through it. Elsewhere the
//! final component is opened with `O_NOFOLLOW` and the opened file is
//! compared with the checked path's by device and inode.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `struct open_how`, which libc marks non-exhaustive.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn c(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL in it"))
    }

    /// `openat2` from `dir` (or the working directory), following no
    /// symlink anywhere in `p`, `/proc` magic links included.
    pub fn open(dir: Option<&OwnedFd>, p: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
        use std::os::fd::AsRawFd;
        let path = c(p)?;
        let how = OpenHow { flags: (flags | libc::O_CLOEXEC) as u64, mode: mode as u64, resolve: libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS };
        let at = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
        // SAFETY: openat2(2) with a valid NUL-terminated path and a
        // correctly sized `open_how`; the result is a new descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_openat2, at, path.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            // A symlink where the check saw none: say what happened.
            return Err(if e.raw_os_error() == Some(libc::ELOOP) { io::Error::other(format!("{} changed into a symlink after it was checked, so it was not opened", p.display())) } else { e });
        }
        // SAFETY: a descriptor openat2 just returned, owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    pub fn rename(dir: &OwnedFd, from: &Path, to: &Path) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let (a, b) = (c(from)?, c(to)?);
        // SAFETY: renameat(2) within one directory descriptor, both names
        // NUL-terminated.
        if unsafe { libc::renameat(dir.as_raw_fd(), a.as_ptr(), dir.as_raw_fd(), b.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn unlink(dir: &OwnedFd, name: &Path) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let n = c(name)?;
        // SAFETY: unlinkat(2) of a name in a directory descriptor; it
        // removes a symlink as a link, never what it leads to.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), n.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `lstat` of a name in a directory descriptor.
    pub fn stat(dir: &OwnedFd, name: &Path) -> io::Result<libc::stat> {
        use std::os::fd::AsRawFd;
        let n = c(name)?;
        // SAFETY: fstatat(2) into a zeroed stat buffer, not following a link.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }
}

fn split(path: &Path) -> io::Result<(&Path, &Path)> {
    match (path.parent(), path.file_name()) {
        (Some(d), Some(n)) => Ok((if d.as_os_str().is_empty() { Path::new(".") } else { d }, Path::new(n))),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} names no file", path.display()))),
    }
}

/// Opens `path` to read, following no symlink.
pub fn open_read(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        Ok(File::from(sys::open(None, path, libc::O_RDONLY | libc::O_NONBLOCK, 0)?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut o = std::fs::OpenOptions::new();
        o.read(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let f = o.open(path)?;
        same_file(&f, path)?;
        Ok(f)
    }
}

/// The opened file is the one at `path` now, not a symlink's target.
#[cfg(not(target_os = "linux"))]
fn same_file(f: &File, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (a, b) = (f.metadata()?, std::fs::symlink_metadata(path)?);
        if b.file_type().is_symlink() || a.dev() != b.dev() || a.ino() != b.ino() {
            return Err(io::Error::other(format!("{} changed after it was checked, so it was not opened", path.display())));
        }
    }
    #[cfg(not(unix))]
    let _ = (f, path);
    Ok(())
}

/// Writes `content` beside `path` as a temporary file, ready for `commit`:
/// made in the parent directory as it was checked, never through a link.
pub fn stage(path: &Path, content: &[u8], tmp_name: &str) -> io::Result<PathBuf> {
    use std::io::Write;
    let (dir, name) = split(path)?;
    let tmp = dir.join(tmp_name);
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let d = sys::open(None, dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let mode = match sys::stat(&d, name) {
            Ok(st) if st.st_mode & libc::S_IFMT == libc::S_IFLNK => return Err(io::Error::other(format!("{} changed into a symlink after it was checked, so it was not written", path.display()))),
            Ok(st) if st.st_mode & 0o222 == 0 => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the file is read-only")),
            Ok(st) => st.st_mode & 0o7777,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0o644,
            Err(e) => return Err(e),
        };
        let fd = sys::open(Some(&d), Path::new(tmp_name), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW, 0o600)?;
        let mut f = File::from(fd);
        let written = (|| {
            f.write_all(content)?;
            // SAFETY: fchmod(2) on the descriptor just made.
            if unsafe { libc::fchmod(f.as_raw_fd(), mode) } != 0 {
                return Err(io::Error::last_os_error());
            }
            f.sync_data()
        })();
        if let Err(e) = written {
            let _ = sys::unlink(&d, Path::new(tmp_name));
            return Err(e);
        }
        Ok(tmp)
    }
    #[cfg(not(target_os = "linux"))]
    {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other(format!("{} changed into a symlink after it was checked, so it was not written", path.display())));
        }
        let _ = name;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(content)?;
        if let Ok(m) = std::fs::metadata(path) {
            f.set_permissions(m.permissions())?;
        }
        f.sync_data()?;
        Ok(tmp)
    }
}

/// Renames a staged file over `path`'s name in its checked directory: a
/// link swapped in meanwhile is replaced, not written through.
pub fn commit(tmp: &Path, path: &Path) -> io::Result<()> {
    let (dir, name) = split(path)?;
    let (_, tmp_name) = split(tmp)?;
    #[cfg(target_os = "linux")]
    {
        let d = sys::open(None, dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        sys::rename(&d, tmp_name, name).inspect_err(|_| {
            let _ = sys::unlink(&d, tmp_name);
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dir, name, tmp_name);
        std::fs::rename(tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(tmp);
        })
    }
}

/// Removes `path`'s name from its checked directory.
pub fn remove(path: &Path) -> io::Result<()> {
    let (dir, name) = split(path)?;
    #[cfg(target_os = "linux")]
    {
        let d = sys::open(None, dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        sys::unlink(&d, name)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dir, name);
        std::fs::remove_file(path)
    }
}
