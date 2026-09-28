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
//! - `outbox` — what the daemon has for one client: one bounded queue per
//!   session, progress frames in keyed slots, and falling behind to a
//!   cursor rather than growing (R-LAG-2, R-LAG-4).
//! - `replay` — the running turn's frames each session keeps for catching
//!   a client up, bounded by a snapshot (R-LAG-10).
//! - `ws` — the WebSocket listener: the same frames in batches, compressed,
//!   windowed, with heartbeats of their own (R-PROTO-1, R-LAG-3/5/9).
//! - `service` — `krowk host enable`: the systemd user unit and the launchd
//!   agent that keep it running on an always-on machine (R-HOST-2).
//!
//! Where it listens: `$XDG_RUNTIME_DIR/krowk/<home>/host.sock`, and where
//! there is no runtime directory (macOS), `$TMPDIR/krowk-<uid>/<home>/…`.
//! `<home>` is the first twelve hex digits of the SHA-256 of krowk's home
//! (`~/.krowk`, or `KROWK_HOME`): a daemon serves one home's config, keys
//! and sessions, so each home gets its own. The directories are `0700` and
//! must be this user's own, the socket `0600`: the socket runs turns with
//! the user's keys, so nobody else may reach it. Beside it, `host.lock` is
//! held by whichever `krowk` is starting the daemon, so two started at once
//! spawn one.

pub mod client;
pub mod outbox;
pub mod remote;
pub mod replay;
pub mod server;
pub mod service;
pub mod ws;

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
    let home = krowk_api::home::resolve(env).map_err(|e| e.fix())?;
    let dir = dir.join(home_key(&home));
    krowk_api::home::make(&dir)?;
    Ok(dir)
}

/// The directory name a home's daemon lives under.
pub fn home_key(home: &Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(home.as_os_str().as_encoded_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// A unix socket's path must fit `sun_path`: 104 bytes on macOS, 108 on
/// Linux, the terminating NUL included.
const SOCKET_PATH_MAX: usize = 103;

/// The socket's path, refused when `sun_path` cannot hold it.
pub fn socket(env: &dyn Fn(&str) -> String) -> Result<PathBuf, String> {
    let path = dir(env)?.join(SOCKET);
    if path.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(format!("{} is too long for a unix socket ({} bytes, at most {SOCKET_PATH_MAX}) — set XDG_RUNTIME_DIR to a shorter directory", path.display(), path.as_os_str().len()));
    }
    Ok(path)
}

/// How long the daemon waits idle before it exits, from `KROWK_HOST_IDLE`
/// (seconds; `0` never exits), else `host.idleMinutes` from config.json
/// (`config`), else ten minutes. None: it never exits by itself, as a
/// service runs it.
pub fn idle_window(env: &dyn Fn(&str) -> String, config: Option<&serde_json::Value>) -> Result<Option<Duration>, String> {
    let secs = match env("KROWK_HOST_IDLE").trim() {
        "" => match config.and_then(|c| c.pointer("/host/idleMinutes")) {
            None => return Ok(Some(DEFAULT_IDLE)),
            // A window under a second would read as none, which is never:
            // refused rather than turned into the opposite of what it says.
            Some(v) => v.as_f64().filter(|m| *m == 0.0 || *m * 60.0 >= 1.0).map(|m| (m * 60.0).round() as u64).ok_or_else(|| format!("host.idleMinutes in config.json is {v}, not a number of minutes of at least one second (0 never exits) — fix it, or remove it for {} minutes", DEFAULT_IDLE.as_secs() / 60))?,
        },
        s => s.parse::<u64>().map_err(|_| format!("KROWK_HOST_IDLE is {s:?}, not a number of seconds — set it to one (0 never exits), or unset it"))?,
    };
    Ok((secs > 0).then(|| Duration::from_secs(secs)))
}

/// Where the WebSocket listener binds, from `KROWK_HOST_WS`, else
/// `host.websocket` in config.json: a loopback `address:port` (port 0
/// picks one; `krowk host status` names it), or `off`. Off when neither
/// says. Anything but loopback is refused: this listener proves who a
/// client is only with the daemon's token, and reaching other devices is
/// the relay's job.
pub fn websocket_addr(env: &dyn Fn(&str) -> String, config: Option<&serde_json::Value>) -> Result<Option<std::net::SocketAddr>, String> {
    let (value, from) = match env("KROWK_HOST_WS").trim() {
        "" => match config.and_then(|c| c.pointer("/host/websocket")) {
            None | Some(serde_json::Value::Null) => return Ok(None),
            Some(v) => (v.as_str().map(str::to_string).ok_or_else(|| format!("host.websocket in config.json is {v}, not an address — set it to \"127.0.0.1:<port>\" or \"off\""))?, "host.websocket in config.json"),
        },
        s => (s.to_string(), "KROWK_HOST_WS"),
    };
    if value == "off" {
        return Ok(None);
    }
    let addr: std::net::SocketAddr = value.parse().map_err(|_| format!("{from} is {value:?}, not an address — set it to 127.0.0.1:<port>, or off"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("{from} is {addr}, which is not loopback — the host daemon's WebSocket listens on this machine only (127.0.0.1:<port>); other devices reach it through the relay"));
    }
    Ok(Some(addr))
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
/// Starts the daemon: the process, when it is a child of this one, so a
/// daemon that exits at start is noticed at once rather than waited for.
pub type Spawn<'a> = dyn Fn() -> Result<Option<std::process::Child>, String> + Send + Sync + 'a;

pub async fn ensure(env: &dyn Fn(&str) -> String, cwd: &Path, version: &str, answers: bool, spawn: &Spawn<'_>) -> Result<client::Client, EngineError> {
    let dir = dir(env).map_err(|e| EngineError::new("host_unavailable", e))?;
    let path = dir.join(SOCKET);
    match client::Client::connect(&path, cwd, version, answers).await {
        Ok(c) => return Ok(c),
        Err(client::ConnectError::Absent) => {}
        Err(client::ConnectError::Failed(e)) => return Err(e),
    }
    let lock = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || Lock::take(&dir)).await.map_err(|e| EngineError::new("host_unavailable", e.to_string()))?.map_err(|e| EngineError::new("host_unavailable", e))?
    };
    match client::Client::connect(&path, cwd, version, answers).await {
        Ok(c) => return Ok(c),
        Err(client::ConnectError::Absent) => {}
        Err(client::ConnectError::Failed(e)) => return Err(e),
    }
    let _ = std::fs::remove_file(&path);
    let mut child = spawn().map_err(|e| EngineError::new("host_unavailable", format!("the host daemon could not be started: {e}")))?;
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT;
    loop {
        match client::Client::connect(&path, cwd, version, answers).await {
            Ok(c) => {
                drop(lock);
                // Reaped when it exits, however long that is, so it never
                // sits as a zombie under a krowk that stays open: a thread
                // blocked in wait(2) costs nothing while it waits.
                if let Some(mut child) = child {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                return Ok(c);
            }
            Err(client::ConnectError::Failed(e)) => return Err(e),
            Err(client::ConnectError::Absent) if tokio::time::Instant::now() >= deadline => {
                return Err(EngineError::new(
                    "host_unavailable",
                    format!("the host daemon did not open {} within {} s — its log says why: `krowk host status`", path.display(), SPAWN_WAIT.as_secs()),
                ));
            }
            Err(client::ConnectError::Absent) => {
                // Gone before it listened: a config it refuses, say. Its
                // log says why; there is nothing to wait for.
                if let Some(Ok(Some(status))) = child.as_mut().map(|c| c.try_wait()) {
                    return Err(EngineError::new("host_unavailable", format!("the host daemon exited as it started ({status}) — ~/.krowk/host.log says why")));
                }
                tokio::time::sleep(Duration::from_millis(2)).await
            }
        }
    }
}

/// Starts `program args…` as the daemon: in a session of its own
/// (`setsid`), so closing the terminal that started it sends it no hangup,
/// with stdin closed and stdout and stderr appended to `log`.
pub fn spawn_detached(program: &Path, args: &[&str], log: &Path) -> Result<std::process::Child, String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let out = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log).map_err(|e| format!("{} cannot be opened: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(program);
    // In `/`, so the daemon pins no directory or mount for its life.
    cmd.args(args).current_dir("/").stdin(std::process::Stdio::null()).stdout(out).stderr(err);
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
    // Not waited for past its start: it outlives this process, and init
    // reaps it once this process is gone.
    cmd.spawn().map_err(|e| format!("{} cannot be run: {e}", program.display()))
}

/// A runtime for the command line's one call, which has none of its own.
fn runtime() -> Result<tokio::runtime::Runtime, EngineError> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}")))
}

/// The daemon's client, when one listens; none when nothing does.
async fn running(env: &dyn Fn(&str) -> String, version: &str) -> Result<Option<client::Client>, EngineError> {
    let socket = socket(env).map_err(|e| EngineError::new("host_unavailable", e))?;
    let cwd = std::env::current_dir().unwrap_or_default();
    match client::Client::connect(&socket, &cwd, version, false).await {
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

/// Stops the daemon that runs, if one does, and waits for its socket to go:
/// `krowk host enable` hands over to the service, which could not bind
/// while it listens. Refused while a turn runs. Answers the pid stopped.
pub fn stop(env: &dyn Fn(&str) -> String, version: &str, force: bool) -> Result<Option<u32>, EngineError> {
    let path = socket(env).map_err(|e| EngineError::new("host_unavailable", e))?;
    runtime()?.block_on(async {
        let Some(c) = running(env, version).await? else { return Ok(None) };
        c.stop(force).await?;
        let pid = c.pid;
        drop(c);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while std::os::unix::net::UnixStream::connect(&path).is_ok() {
            if tokio::time::Instant::now() >= deadline {
                return Err(EngineError::new("host_busy", format!("the host daemon (pid {pid}) did not exit — `kill {pid}`, then try again")));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(Some(pid))
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
                    // A result is never part of the catching up (it is a
                    // live frame, after the log's `turn.completed`), so one
                    // that lands in the same read as `attached` is the end.
                    if matches!(&line, StreamLine::Live(LiveEvent::Result(r)) if r.session_id == session) {
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
        let e = [("XDG_RUNTIME_DIR", root.display().to_string()), ("HOME", "/home/ada".to_string())];
        let key = home_key(Path::new("/home/ada/.krowk"));
        assert_eq!(socket(&env(&e)).unwrap(), root.join("krowk").join(&key).join("host.sock"));
        assert_eq!(std::fs::metadata(root.join("krowk")).unwrap().permissions().mode() & 0o777, 0o700);
        // macOS: no runtime directory, the per-user TMPDIR instead.
        let e = [("TMPDIR", root.display().to_string()), ("HOME", "/home/ada".to_string())];
        let uid = unsafe { libc::getuid() };
        assert_eq!(socket(&env(&e)).unwrap(), root.join(format!("krowk-{uid}")).join(&key).join("host.sock"));
        // Another home is another daemon: its config, keys and sessions.
        let e = [("XDG_RUNTIME_DIR", root.display().to_string()), ("HOME", "/home/ada".to_string()), ("KROWK_HOME", "/work/.krowk".to_string())];
        assert_ne!(socket(&env(&e)).unwrap().parent().unwrap().file_name().unwrap().to_str().unwrap(), key);
        // One made loose by someone is made private again, and a symlink —
        // which could lead anywhere — is refused.
        std::fs::set_permissions(root.join("krowk"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = [("XDG_RUNTIME_DIR", root.display().to_string()), ("HOME", "/home/ada".to_string())];
        socket(&env(&e)).unwrap();
        assert_eq!(std::fs::metadata(root.join("krowk")).unwrap().permissions().mode() & 0o777, 0o700);
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(root.join("krowk"), other.join("krowk")).unwrap();
        let e = [("XDG_RUNTIME_DIR", other.display().to_string()), ("HOME", "/home/ada".to_string())];
        assert!(socket(&env(&e)).unwrap_err().contains("symlink"));
        // One sun_path cannot hold is refused by name, not left to bind.
        let deep = root.join("d".repeat(90));
        std::fs::create_dir(&deep).unwrap();
        let e = [("XDG_RUNTIME_DIR", deep.display().to_string()), ("HOME", "/home/ada".to_string())];
        assert!(socket(&env(&e)).unwrap_err().contains("too long for a unix socket"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r_proto_1_the_websocket_listens_on_loopback_or_not_at_all() {
        let none: [(&str, String); 0] = [];
        assert_eq!(websocket_addr(&env(&none), None).unwrap(), None, "off by default");
        let cfg = serde_json::json!({"host": {"websocket": "127.0.0.1:7788"}});
        assert_eq!(websocket_addr(&env(&none), Some(&cfg)).unwrap(), Some("127.0.0.1:7788".parse().unwrap()));
        let e = [("KROWK_HOST_WS", "off".to_string())];
        assert_eq!(websocket_addr(&env(&e), Some(&cfg)).unwrap(), None);
        let e = [("KROWK_HOST_WS", "[::1]:0".to_string())];
        assert!(websocket_addr(&env(&e), None).unwrap().is_some());
        for wide in ["0.0.0.0:7788", "192.168.1.2:7788"] {
            let e = [("KROWK_HOST_WS", wide.to_string())];
            assert!(websocket_addr(&env(&e), None).unwrap_err().contains("not loopback"), "{wide}");
        }
        let e = [("KROWK_HOST_WS", "localhost".to_string())];
        assert!(websocket_addr(&env(&e), None).unwrap_err().contains("not an address"));
    }

    /// R-LAG-9's tripwire for the daemon's own serving code: no blocking
    /// call there unless marked `// blocking:` with why, above the
    /// statement or function it covers — at start, on the way out, or
    /// inside `spawn_blocking`. A substring scan, so a tripwire, not the
    /// enforcement: that is the lateness probe in `tests/daemon_ws.rs`,
    /// which fails when anything — here, in the host, in a tool — holds the
    /// daemon's thread past 30 ms while sessions stream.
    #[test]
    fn r_lag_9_no_blocking_io_on_the_daemons_thread() {
        const BLOCKING: [&str; 11] = ["std::fs::", "std::thread::sleep", "std::os::unix::net::", "std::io::stdin", "std::net::TcpStream", "File::open", "File::create", "OpenOptions::new", "read_events(", ".block_on(", "sync_data("];
        let files = [("server.rs", include_str!("server.rs")), ("outbox.rs", include_str!("outbox.rs")), ("replay.rs", include_str!("replay.rs")), ("ws.rs", include_str!("ws.rs"))];
        let mut found = Vec::new();
        for (name, src) in files {
            // The tests below each file block as they like.
            let src = src.split("#[cfg(test)]").next().unwrap();
            let (mut covered, mut depth, mut marked) = (false, 0i32, false);
            for (n, line) in src.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("// blocking:") {
                    marked = true;
                    continue;
                }
                if marked && !code.starts_with("//") && !code.starts_with("#[") {
                    (covered, depth, marked) = (true, 0, false);
                }
                let hit = BLOCKING.iter().any(|b| code.contains(b)) && !code.starts_with("//") && !code.starts_with("use ");
                // Waited for off the thread, in the one statement.
                if hit && !covered && !code.contains("spawn_blocking(") && !(name == "server.rs" && code.contains("local.block_on(&rt")) {
                    found.push(format!("{name}:{}: {code}", n + 1));
                }
                if covered {
                    depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                    if depth <= 0 && (code.ends_with(';') || code.ends_with('}') || code.ends_with("})")) {
                        covered = false;
                    }
                }
            }
        }
        assert!(found.is_empty(), "blocking I/O on the daemon's thread — move it into spawn_blocking, or mark why it may block:\n{}", found.join("\n"));
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
        let tiny = serde_json::json!({"host": {"idleMinutes": 0.005}});
        assert!(idle_window(&env(&none), Some(&tiny)).unwrap_err().contains("at least one second"), "not rounded down to never");
        let e = [("KROWK_HOST_IDLE", "soon".to_string())];
        assert!(idle_window(&env(&e), None).unwrap_err().contains("KROWK_HOST_IDLE"));
    }
}
