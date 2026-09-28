//! The host daemon: one per user, holding every session's `Host` so a
//! session outlives the terminal that started it (R-HOST-1). Clients reach
//! it over a unix socket and speak the in-process protocol's own types —
//! `Command` in, `StreamLine` out — wrapped in `ClientFrame` and
//! `ServerFrame`, one JSON object a line (R-PROTO-1's socket transport).
//!
//! - `server` — the listener around `Host`, the fan-out to every client
//!   following a session, catching a late client up, and the idle exit.
//! - `client` — connecting, the hello, and `execute` and `attach` over the
//!   socket, with the same signature `Host::execute` has.
//! - `service` — `krowk host enable`: the systemd user unit and the launchd
//!   agent that keep it running on an always-on machine (R-HOST-2).
//!
//! Where it listens: `$XDG_RUNTIME_DIR/krowk/host.sock`, and where there is
//! no runtime directory (macOS), `$TMPDIR/krowk-<uid>/host.sock`. The
//! directory is `0700` and must be this user's own, the socket `0600`: the
//! socket runs turns with the user's keys, so nobody else may reach it.
//! Beside it, `host.lock` is held by whichever `krowk` is starting the
//! daemon, so two started at once spawn one.

pub mod client;
pub mod server;
pub mod service;

use crate::engine::EngineError;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SOCKET: &str = "host.sock";
pub const LOCK: &str = "host.lock";

/// How long the daemon stays up with no turn running and no client
/// connected, unless `KROWK_HOST_IDLE` or config says otherwise.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(10 * 60);

/// How long a `krowk` that spawned the daemon waits for its socket.
const SPAWN_WAIT: Duration = Duration::from_secs(5);

/// The daemon's directory: made `0700` when it is not there, and refused
/// when it is a symlink or another user's — the same rules krowk's home is
/// held to (`krowk_api::home::own`).
pub fn dir(env: &dyn Fn(&str) -> String) -> Result<PathBuf, String> {
    let runtime = env("XDG_RUNTIME_DIR");
    let dir = if !runtime.is_empty() && Path::new(&runtime).is_absolute() {
        PathBuf::from(runtime).join("krowk")
    } else {
        let tmp = env("TMPDIR");
        let base = if !tmp.is_empty() && Path::new(&tmp).is_absolute() { PathBuf::from(tmp) } else { PathBuf::from("/tmp") };
        // SAFETY: getuid has no preconditions and cannot fail.
        base.join(format!("krowk-{}", unsafe { libc::getuid() }))
    };
    krowk_api::home::make(&dir)?;
    Ok(dir)
}

/// The socket's path.
pub fn socket(env: &dyn Fn(&str) -> String) -> Result<PathBuf, String> {
    Ok(dir(env)?.join(SOCKET))
}

/// How long the daemon waits idle before it exits, from `KROWK_HOST_IDLE`
/// (seconds; `0` never exits), else `host.idleMinutes` from config.json
/// (`config`), else ten minutes. None: it never exits by itself, as a
/// service runs it.
pub fn idle_window(env: &dyn Fn(&str) -> String, config: Option<&serde_json::Value>) -> Result<Option<Duration>, String> {
    let secs = match env("KROWK_HOST_IDLE").trim() {
        "" => match config.and_then(|c| c.pointer("/host/idleMinutes")) {
            None => return Ok(Some(DEFAULT_IDLE)),
            Some(v) => v.as_f64().filter(|m| *m >= 0.0).map(|m| (m * 60.0) as u64).ok_or_else(|| format!("host.idleMinutes in config.json is {v}, not a number of minutes — fix it, or remove it for {} minutes", DEFAULT_IDLE.as_secs() / 60))?,
        },
        s => s.parse::<u64>().map_err(|_| format!("KROWK_HOST_IDLE is {s:?}, not a number of seconds — set it to one (0 never exits), or unset it"))?,
    };
    Ok((secs > 0).then(|| Duration::from_secs(secs)))
}

/// The lock a starting `krowk` holds while it checks for a daemon and
/// spawns one: `flock` on `host.lock`, let go when dropped (or when the
/// process dies, so a crashed starter never wedges the next).
pub struct Lock {
    _held: std::fs::File,
}

impl Lock {
    pub fn take(dir: &Path) -> Result<Lock, String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join(LOCK);
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&path).map_err(|e| format!("{} cannot be opened: {e}", path.display()))?;
        // SAFETY: the descriptor is the open file's and outlives the call.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("{} cannot be locked: {}", path.display(), std::io::Error::last_os_error()));
        }
        Ok(Lock { _held: f })
    }
}

/// Whether a connect error means nothing listens there: no socket, or one
/// a daemon that died left behind.
pub(crate) fn absent(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
}

/// The daemon's client, connecting to the one running or — when none is —
/// starting it with `spawn` and waiting for its socket. Under the lock, so
/// of two `krowk`s started together one spawns and the other connects to
/// what it spawned; a socket a dead daemon left is removed only under it.
pub async fn ensure(env: &dyn Fn(&str) -> String, cwd: &Path, version: &str, spawn: &dyn Fn() -> Result<(), String>) -> Result<client::Client, EngineError> {
    let dir = dir(env).map_err(|e| EngineError::new("host_unavailable", e))?;
    let path = dir.join(SOCKET);
    match client::Client::connect(&path, cwd, version).await {
        Ok(c) => return Ok(c),
        Err(client::ConnectError::Absent) => {}
        Err(client::ConnectError::Failed(e)) => return Err(e),
    }
    let lock = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || Lock::take(&dir)).await.map_err(|e| EngineError::new("host_unavailable", e.to_string()))?.map_err(|e| EngineError::new("host_unavailable", e))?
    };
    match client::Client::connect(&path, cwd, version).await {
        Ok(c) => return Ok(c),
        Err(client::ConnectError::Absent) => {}
        Err(client::ConnectError::Failed(e)) => return Err(e),
    }
    let _ = std::fs::remove_file(&path);
    spawn().map_err(|e| EngineError::new("host_unavailable", format!("the host daemon could not be started: {e}")))?;
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT;
    loop {
        match client::Client::connect(&path, cwd, version).await {
            Ok(c) => {
                drop(lock);
                return Ok(c);
            }
            Err(client::ConnectError::Failed(e)) => return Err(e),
            Err(client::ConnectError::Absent) if tokio::time::Instant::now() >= deadline => {
                return Err(EngineError::new(
                    "host_unavailable",
                    format!("the host daemon did not open {} within {} s — its log says why: `krowk host status`", path.display(), SPAWN_WAIT.as_secs()),
                ));
            }
            Err(client::ConnectError::Absent) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// Starts `program args…` as the daemon: in a session of its own
/// (`setsid`), so closing the terminal that started it sends it no hangup,
/// with stdin closed and stdout and stderr appended to `log`.
pub fn spawn_detached(program: &Path, args: &[&str], log: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let out = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log).map_err(|e| format!("{} cannot be opened: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    // SAFETY: setsid is async-signal-safe, and nothing else runs between
    // fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Not waited for: it outlives this process. Its exit is reaped by init
    // once this process is gone, and before then it runs for minutes.
    cmd.spawn().map(drop).map_err(|e| format!("{} cannot be run: {e}", program.display()))
}

/// A runtime for the command line's one call, which has none of its own.
fn runtime() -> Result<tokio::runtime::Runtime, EngineError> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}")))
}

/// The daemon's client, when one listens; none when nothing does.
async fn running(env: &dyn Fn(&str) -> String, version: &str) -> Result<Option<client::Client>, EngineError> {
    let socket = socket(env).map_err(|e| EngineError::new("host_unavailable", e))?;
    let cwd = std::env::current_dir().unwrap_or_default();
    match client::Client::connect(&socket, &cwd, version).await {
        Ok(c) => Ok(Some(c)),
        Err(client::ConnectError::Absent) => Ok(None),
        Err(client::ConnectError::Failed(e)) => Err(e),
    }
}

/// `krowk host status`: how the daemon is, or none when none runs. Never
/// starts one.
pub fn status(env: &dyn Fn(&str) -> String, version: &str) -> Result<Option<crate::protocol::HostStatus>, EngineError> {
    runtime()?.block_on(async {
        match running(env, version).await? {
            Some(c) => c.status().await.map(Some),
            None => Ok(None),
        }
    })
}

/// `krowk host attach`: every frame of `session` as stream-json — what its
/// log holds, the running turn so far, then live until that turn's result.
/// A session with no turn running prints its log and ends.
pub fn attach(env: &dyn Fn(&str) -> String, version: &str, session: &str, out: &mut dyn std::io::Write) -> Result<(), EngineError> {
    use crate::protocol::{LiveEvent, StreamLine};
    let mut print = |line: &StreamLine| {
        let _ = writeln!(out, "{}", serde_json::to_string(line).expect("a stream line serializes"));
        let _ = out.flush();
    };
    runtime()?.block_on(async {
        let Some(c) = running(env, version).await? else {
            return Err(EngineError::new("host_not_running", format!("no host daemon runs, so session {session} is not running there — `krowk -p --resume {session} …` continues it")));
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamLine>(1024);
        let attach = c.attach(session, None, tx);
        tokio::pin!(attach);
        let mut live = false;
        loop {
            tokio::select! {
                biased;
                Some(line) = rx.recv() => {
                    print(&line);
                    if live && matches!(&line, StreamLine::Live(LiveEvent::Result(r)) if r.session_id == session) {
                        return Ok(());
                    }
                }
                r = &mut attach, if !live => {
                    if !r? {
                        // Caught up, and nothing more is coming.
                        while let Ok(line) = rx.try_recv() {
                            print(&line);
                        }
                        return Ok(());
                    }
                    live = true;
                }
                else => return Err(EngineError::new("host_gone", "the host daemon closed the connection before the turn ended — `krowk host status` says whether it is up")),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> String + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("krowk-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn r_host_1_the_socket_is_in_the_runtime_dir_or_a_private_tmp_one() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("paths");
        let e = [("XDG_RUNTIME_DIR", root.display().to_string())];
        assert_eq!(socket(&env(&e)).unwrap(), root.join("krowk/host.sock"));
        assert_eq!(std::fs::metadata(root.join("krowk")).unwrap().permissions().mode() & 0o777, 0o700);
        // macOS: no runtime directory, the per-user TMPDIR instead.
        let e = [("TMPDIR", root.display().to_string())];
        let uid = unsafe { libc::getuid() };
        assert_eq!(socket(&env(&e)).unwrap(), root.join(format!("krowk-{uid}/host.sock")));
        // One made loose by someone is made private again, and a symlink —
        // which could lead anywhere — is refused.
        std::fs::set_permissions(root.join("krowk"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = [("XDG_RUNTIME_DIR", root.display().to_string())];
        socket(&env(&e)).unwrap();
        assert_eq!(std::fs::metadata(root.join("krowk")).unwrap().permissions().mode() & 0o777, 0o700);
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(root.join("krowk"), other.join("krowk")).unwrap();
        let e = [("XDG_RUNTIME_DIR", other.display().to_string())];
        assert!(socket(&env(&e)).unwrap_err().contains("symlink"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r_host_1_the_idle_window_comes_from_the_env_then_config() {
        let none: [(&str, String); 0] = [];
        assert_eq!(idle_window(&env(&none), None).unwrap(), Some(DEFAULT_IDLE));
        let cfg = serde_json::json!({"host": {"idleMinutes": 2}});
        assert_eq!(idle_window(&env(&none), Some(&cfg)).unwrap(), Some(Duration::from_secs(120)));
        let e = [("KROWK_HOST_IDLE", "3".to_string())];
        assert_eq!(idle_window(&env(&e), Some(&cfg)).unwrap(), Some(Duration::from_secs(3)));
        let e = [("KROWK_HOST_IDLE", "0".to_string())];
        assert_eq!(idle_window(&env(&e), None).unwrap(), None, "a service never exits by itself");
        let e = [("KROWK_HOST_IDLE", "soon".to_string())];
        assert!(idle_window(&env(&e), None).unwrap_err().contains("KROWK_HOST_IDLE"));
    }
}
