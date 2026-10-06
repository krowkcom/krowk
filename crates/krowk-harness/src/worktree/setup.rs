//! The setup command (WT8): the last prepare step. Some projects need a
//! step before an agent can work in a fresh checkout — install
//! dependencies, generate code, create a database — and an agent that
//! is not told finds out by failing. `worktrees.setup` names that step,
//! a shell command, run in the new worktree before the agent's first turn.
//!
//! - **Whose command**: the repository's own, in its `.krowk/config.json`,
//!   only once the person trusts the repository (`crate::trust`, the
//!   question that also turns its hooks on), and then it is the one that
//!   runs; the person's, in krowk's `config.json`, otherwise. An untrusted
//!   repository's command is not run, and the agent is told so.
//! - **In the sandbox**: the workspace profile over the worktree — the
//!   network on, since installing is most of what it is for, and only the
//!   worktree writable (with the worktree's own git files a commit writes,
//!   WT4's) — so a cloned repository's `npm install` cannot reach the main
//!   checkout, the person's home or anything else. Where this machine has
//!   no sandbox it runs unsandboxed, and so only for a trusted repository,
//!   whoever's command it is: an install runs the repository's code.
//!   Inside a container the container holds it, as it holds the agent's
//!   commands.
//! - **Its environment**: `KROWK_PROJECT_ROOT` (the main checkout),
//!   `KROWK_WORKTREE_PATH` and `KROWK_PORT_BASE`; in the sandbox, on top of
//!   its allowlist. The port base is the worktree's port slot's
//!   (`port_slot`), which the agent's own commands see too, so servers
//!   started in two worktrees at once do not fight over a port.
//! - **A build slot** (`crate::builds`) when the command is a heavy one, as
//!   the bash tool takes for it, waited for within the timeout.
//! - **Its output** goes to `<admin dir>/krowk-setup.log`, the worktree's
//!   own directory in the repository's git directory, written by krowk
//!   once the command is over — and so once nothing it started is left
//!   to plant a link there (the sandbox's pid namespace is gone) — and
//!   never through a link. Kept to its last `LOG_CAP` bytes.
//! - **A failure stops nothing**: a non-zero exit, or `setupTimeout`
//!   (600 s by default) passing, which stops it and all it started, puts
//!   a note at the top of the agent's first prompt with the exit code or
//!   "timed out" and the output's last 50 lines; the agent starts anyway.

use super::{Prepare, query, read};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// `setupTimeout` when the config has none: seconds.
pub const DEFAULT_TIMEOUT: u64 = 600;
/// The setup command's output, in the worktree's admin directory.
pub const LOG_FILE: &str = "krowk-setup.log";
/// How much of the output the log keeps: the end of it.
const LOG_CAP: usize = 1 << 20;
/// What of the output a note quotes: its last lines, and at most so many
/// bytes of them, so a line of a megabyte cannot fill the prompt.
const NOTE_LINES: usize = 50;
const NOTE_BYTES: usize = 8 * 1024;

/// The pool every live krowk worktree holds a slot of (`crate::slots`).
pub const PORT_POOL: &str = "port-slots";
/// Its size: port bases from 20000 to 29990.
pub const PORT_SLOTS: usize = 1000;

/// A port slot for a new worktree, the lowest free one: held for as long
/// as the worktree is in use, and let go when it is dropped. None when
/// every one is held.
pub fn port_slot(runtime: PathBuf) -> Result<Option<crate::slots::Slot>, String> {
    crate::slots::Pool::new(runtime, PORT_POOL, PORT_SLOTS).try_take()
}

/// `KROWK_PORT_BASE` for the port slot `slot`: `20000 + 10 n`, ten ports
/// to each worktree.
pub fn port_base(slot: &crate::slots::Slot) -> u16 {
    20000 + 10 * slot.index().min(PORT_SLOTS - 1) as u16
}

/// What the setup command of the worktree whose top is `worktree` runs
/// in: the workspace profile over it alone, with `policy`'s home, fences
/// and secrets, held by bubblewrap where it works, else by the container
/// krowk runs in. None where this machine has neither.
pub fn sandbox(policy: &crate::permissions::Policy, worktree: &Path) -> Option<crate::sandbox::Plan> {
    use crate::sandbox::{By, Plan, Profile, Sandbox};
    let by = if crate::sandbox::enforcer().is_ok() {
        By::Bubblewrap
    } else if crate::sandbox::in_container() {
        By::Container
    } else {
        return None;
    };
    Some(Plan::new(Sandbox { profile: Profile::Workspace, by }, worktree, &[], &policy.read_dirs, &policy.protected, &policy.secrets, policy.home.as_deref()))
}

/// The setup step (see the module docs).
pub(super) fn setup(p: &Prepare<'_>) -> Option<String> {
    let mut notes = Vec::new();
    let own = p.project.filter(|w| w.setup().is_some());
    let (config, from) = match own {
        Some(own) if p.trusted => (own, "the repository's .krowk/config.json"),
        _ => {
            if own.is_some() {
                notes.push("the repository's worktrees.setup command (in its .krowk/config.json) did not run: the person has not trusted this repository, and krowk runs a repository's own commands only in one they trust. Set up what your task needs yourself.".to_string());
            }
            (p.config, "krowk's config.json")
        }
    };
    if let Some(command) = config.setup() {
        notes.extend(run_setup(p, command, from, config.setup_timeout()));
    }
    (!notes.is_empty()).then(|| notes.join("\n\n"))
}

/// How a setup command ended.
#[derive(Debug, PartialEq)]
enum Ended {
    Exited(i32),
    Signal(i32),
    TimedOut,
    NotStarted(String),
}

/// Runs `command` from `from` in the worktree, logs it, and says what went
/// wrong, if anything.
fn run_setup(p: &Prepare<'_>, command: &str, from: &str, timeout: Duration) -> Option<String> {
    let wt = p.worktree;
    let gate = |why: &str| Some(format!("the worktree's setup command (`{command}`, from {from}) did not run: {why}. Set up what your task needs yourself."));
    let plan = match p.sandbox {
        Some(plan) => match &plan.refused {
            Some(why) => return gate(why),
            None => Some(plan),
        },
        None if p.trusted => None,
        None => return gate("this machine has no sandbox to run it in, and outside one krowk runs it only for a repository the person has trusted"),
    };
    let deadline = Instant::now() + timeout;
    let mut env: Vec<(String, String)> = vec![("KROWK_PROJECT_ROOT".into(), wt.main.display().to_string()), ("KROWK_WORKTREE_PATH".into(), wt.path.display().to_string())];
    env.extend(p.port_base.map(|b| ("KROWK_PORT_BASE".to_string(), b.to_string())));
    // A heavy command waits for a build slot as the bash tool's would,
    // within its own time.
    let mut ended = None;
    let builds = p.builds.filter(|_| crate::builds::heavy(command));
    env.extend(builds.and_then(|b| b.jobs.clone()).map(|n| ("CARGO_BUILD_JOBS".to_string(), n)));
    let _slot = match builds.and_then(|b| b.pool.as_ref()) {
        Some(pool) => loop {
            match pool.try_take() {
                Ok(Some(slot)) => break Some(slot),
                // A pool that cannot be had runs it anyway, as bash does.
                Err(_) => break None,
                Ok(None) if Instant::now() >= deadline => {
                    ended = Some(Ended::TimedOut);
                    break None;
                }
                Ok(None) => std::thread::sleep(crate::slots::RETRY),
            }
        },
        None => None,
    };
    let (ended, output, removed) = match ended {
        Some(e) => (e, b"(it was still waiting for a build slot)\n".to_vec(), Vec::new()),
        None => run_in(plan, command, &wt.path, &env, deadline),
    };
    let how = match &ended {
        Ended::Exited(0) => None,
        Ended::Exited(code) => Some(format!("exited with code {code}")),
        Ended::Signal(n) => Some(format!("was killed by signal {n}")),
        Ended::TimedOut => Some(format!("timed out after {} s and was stopped", timeout.as_secs())),
        Ended::NotStarted(why) => Some(format!("could not be started: {why}")),
    };
    let text = String::from_utf8_lossy(&output);
    let mut log = format!("$ {command}\n{text}");
    if !log.ends_with('\n') {
        log.push('\n');
    }
    for r in &removed {
        log.push_str(&format!("the sandbox removed {}, which the command created: git and agents run what such a directory names\n", r.display()));
    }
    log.push_str(&format!("({})\n", how.as_deref().unwrap_or("exited with code 0")));
    let logged = write_log(wt, &log);
    let how = how?;
    let at = match logged {
        Ok(at) => format!("Its output is in {}", at.display()),
        Err(e) => format!("Its output could not be kept ({e})"),
    };
    Some(format!("the worktree's setup command (`{command}`, from {from}) {how}, so the worktree may not be ready for your task. {at}; its last lines:\n```\n{}\n```", tail(&text)))
}

/// The last `NOTE_LINES` lines of `text`, kept to their last `NOTE_BYTES`.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let last = lines[lines.len().saturating_sub(NOTE_LINES)..].join("\n");
    if last.len() <= NOTE_BYTES {
        return last;
    }
    let mut start = last.len() - NOTE_BYTES;
    while !last.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &last[start..])
}

/// Runs `command` in `dir` — in `plan`'s sandbox, or as it is when there is
/// none — until it ends or `deadline` passes, with stdout and stderr on one
/// pipe in the order they were written. How it ended, the end of its
/// output, and what the sandbox removed after it.
fn run_in(plan: Option<&crate::sandbox::Plan>, command: &str, dir: &Path, env: &[(String, String)], deadline: Instant) -> (Ended, Vec<u8>, Vec<PathBuf>) {
    let not_started = |e: String| (Ended::NotStarted(e), Vec::new(), Vec::new());
    let mut cmd = match plan.filter(|p| p.kernel).map(|p| crate::sandbox::bash(p, command, env)) {
        None => {
            let mut c = Command::new("bash");
            c.arg("-c").arg(command).envs(env.iter().cloned());
            c
        }
        Some(Ok((program, args))) => {
            let mut c = Command::new(program);
            // bubblewrap's own environment is the allowlist too.
            c.args(args).env_clear().envs(crate::sandbox::env());
            c
        }
        Some(Err(fix)) => return not_started(fix),
    };
    let mut unfenced = plan.filter(|p| p.kernel).map(crate::sandbox::Unfenced::before);
    let r = match std::io::pipe().and_then(|(r, w)| Ok((r, w.try_clone()?, w))) {
        Ok((r, w1, w2)) => {
            cmd.stdout(w1).stderr(w2);
            r
        }
        Err(e) => return not_started(e.to_string()),
    };
    cmd.current_dir(dir).stdin(std::process::Stdio::null());
    // No terminal: an install that would ask on `/dev/tty` fails rather
    // than draw over krowk's. Its session is its group, stopped whole.
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and touches no memory of the
    // parent's; it runs in the child between fork and exec.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let child = cmd.spawn();
    // The command's copies of the write end go with it, so the pipe closes
    // when it and what it started are done.
    drop(cmd);
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return not_started(e.to_string()),
    };
    crate::group::register(Some(child.id()));
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut r, mut kept, mut cut, mut chunk) = (r, Vec::new(), 0usize, [0u8; 8192]);
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&chunk[..n]);
            if kept.len() > 2 * LOG_CAP {
                let over = kept.len() - LOG_CAP;
                kept.drain(..over);
                cut += over;
            }
        }
        if kept.len() > LOG_CAP {
            cut += kept.len() - LOG_CAP;
            kept.drain(..kept.len() - LOG_CAP);
        }
        if cut > 0 {
            let mut marked = format!("[… the first {cut} bytes of the output are not kept …]\n").into_bytes();
            marked.extend(kept);
            kept = marked;
        }
        let _ = done.send(kept);
    });
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // What it left running in its session, holding the pipe,
                // goes with it.
                crate::readiness::kill_group(&child);
                crate::group::release(Some(child.id()));
                break exit(status);
            }
            Ok(None) if Instant::now() >= deadline => {
                crate::readiness::stop(&mut child);
                break Ended::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                crate::readiness::stop(&mut child);
                break Ended::NotStarted(e.to_string());
            }
        }
    };
    // A process that left its session can hold the pipe for good: read
    // on only briefly once the command is over.
    let output = finished.recv_timeout(Duration::from_secs(1)).unwrap_or_else(|_| b"(its output could not be read to the end: a process it started still holds it)\n".to_vec());
    let removed = unfenced.as_mut().map(|u| u.appeared()).unwrap_or_default();
    (ended, output, removed)
}

fn exit(status: std::process::ExitStatus) -> Ended {
    #[cfg(unix)]
    if let Some(n) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return Ended::Signal(n);
    }
    Ended::Exited(status.code().unwrap_or(-1))
}

/// Writes `text` to the worktree's `krowk-setup.log`, in its admin
/// directory: a file the command may have put there first is replaced,
/// and the file is made new, never opened through a link. Where it is.
fn write_log(wt: &super::Worktree, text: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let admin = PathBuf::from(read(query(&wt.path).map_err(|e| e.to_string())?.args(["rev-parse", "--absolute-git-dir"]), "rev-parse").map_err(|e| e.to_string())?);
    if !admin.starts_with(wt.common.join("worktrees")) {
        return Err(format!("{} is not the worktree's own git directory", admin.display()));
    }
    let at = admin.join(LOG_FILE);
    if at.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        std::fs::remove_file(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o.open(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", at.display()))?;
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_ok, has_git, repo_in};
    use super::super::{Finished, Worktree, create, finish, first_prompt, prepare};
    use super::*;
    use crate::instances::WorktreesConfig;

    /// A scratch directory outside `/tmp`, which the sandbox replaces with
    /// a private one.
    fn scratch(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("krowk-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn setup_config(command: &str, timeout: Option<u64>) -> WorktreesConfig {
        WorktreesConfig { setup: Some(command.into()), setup_timeout: timeout, ..WorktreesConfig::default() }
    }

    /// The worktree's setup log.
    fn log_of(w: &Worktree) -> PathBuf {
        PathBuf::from(git_ok(&w.path, &["rev-parse", "--absolute-git-dir"])).join(LOG_FILE)
    }

    /// A trusted repository's own command runs in the worktree, with the
    /// port base of the slot each worktree holds — two live worktrees,
    /// two bases — before anything else could start there, and its output
    /// is logged in the worktree's admin directory, through no link.
    #[test]
    fn wt8_a_trusted_projects_setup_runs_with_its_own_port_base() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let d = scratch("trusted");
        let (_, main, root) = repo_in(&d, "trusted");
        let project = setup_config("echo $KROWK_PORT_BASE > port && touch ready && echo set up", None);
        let user = setup_config("touch from-user", None);
        let (a, b) = (create(&main, &root, "s1").unwrap(), create(&main, &root, "s2").unwrap());
        let outside = d.join("outside.txt");
        std::fs::write(&outside, "untouched\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, log_of(&a)).unwrap();
        let (sa, sb) = (port_slot(d.join("run")).unwrap().unwrap(), port_slot(d.join("run")).unwrap().unwrap());
        for (w, slot) in [(&a, &sa), (&b, &sb)] {
            let plan = sandbox(&crate::permissions::Policy::default(), &w.path);
            let p = Prepare { project: Some(&project), trusted: true, sandbox: plan.as_ref(), port_base: Some(port_base(slot)), ..Prepare::new(w, &user) };
            assert_eq!(prepare(&p), Vec::<String>::new());
            assert!(w.path.join("ready").is_file());
            assert!(!w.path.join("from-user").exists(), "the repository's command is the one that runs");
            assert_eq!(std::fs::read_to_string(w.path.join("port")).unwrap().trim(), port_base(slot).to_string());
            let log = std::fs::read_to_string(log_of(w)).unwrap();
            assert!(log.starts_with("$ echo $KROWK_PORT_BASE") && log.contains("set up\n") && log.ends_with("(exited with code 0)\n"), "{log}");
        }
        assert_eq!((port_base(&sa), port_base(&sb)), (20000, 20010));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched\n", "the log replaced the link rather than writing through it");
        // Its files are the agent's changes: the worktree is kept.
        assert!(matches!(finish(&a).unwrap(), Finished::Kept { .. }));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An untrusted repository's command does not run, and the agent is
    /// told; the person's own runs, in the sandbox — and, where there is
    /// none, not for an untrusted repository either.
    #[test]
    fn wt8_an_untrusted_projects_setup_does_not_run() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let d = scratch("untrusted");
        let (_, main, root) = repo_in(&d, "untrusted");
        let project = setup_config("touch from-project", None);
        let w = create(&main, &root, "s1").unwrap();
        let notes = prepare(&Prepare { project: Some(&project), ..Prepare::new(&w, &WorktreesConfig::default()) });
        assert!(!w.path.join("from-project").exists());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("worktrees.setup command (in its .krowk/config.json) did not run") && notes[0].contains("not trusted"), "{notes:?}");
        // The person's own, with no sandbox to run it in: not for an
        // untrusted repository.
        let user = setup_config("touch from-user", None);
        let notes = prepare(&Prepare::new(&w, &user));
        assert!(!w.path.join("from-user").exists());
        assert!(notes[0].contains("did not run: this machine has no sandbox"), "{notes:?}");
        if crate::sandbox::enforcer().is_ok() {
            let plan = sandbox(&crate::permissions::Policy::default(), &w.path);
            assert_eq!(prepare(&Prepare { sandbox: plan.as_ref(), ..Prepare::new(&w, &user) }), Vec::<String>::new());
            assert!(w.path.join("from-user").is_file(), "the person's own runs in the sandbox");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// (bubblewrap) The command can write the worktree and nothing else:
    /// not the main checkout.
    #[test]
    fn wt8_a_setup_command_cannot_write_the_main_checkout() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        if let Err(why) = crate::sandbox::enforcer() {
            assert!(std::env::var_os("CI").is_none(), "CI on Linux must run the sandbox's tests, and {why}");
            eprintln!("skipped: {why}");
            return;
        }
        let d = scratch("fenced");
        let (_, main, root) = repo_in(&d, "fenced");
        let w = create(&main, &root, "s1").unwrap();
        let user = setup_config("touch here && echo changed > \"$KROWK_PROJECT_ROOT/a.txt\"", None);
        let plan = sandbox(&crate::permissions::Policy::default(), &w.path);
        let notes = prepare(&Prepare { sandbox: plan.as_ref(), trusted: true, ..Prepare::new(&w, &user) });
        assert!(w.path.join("here").is_file(), "the worktree is writable");
        assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "a\n", "the main checkout is not");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("exited with code 1") && notes[0].contains("Read-only file system"), "{notes:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A command that fails, and one that runs past its time, each leave
    /// a note at the top of the child's first prompt: the exit code or
    /// "timed out", and the end of the output.
    #[test]
    fn wt8_a_failing_or_slow_setup_is_a_note_in_the_first_prompt() {
        if !has_git() {
            eprintln!("git is not installed: skipping");
            return;
        }
        let d = scratch("fails");
        let (_, main, root) = repo_in(&d, "fails");
        let w = create(&main, &root, "s1").unwrap();
        let plan = sandbox(&crate::permissions::Policy::default(), &w.path);
        let run = |cfg: &WorktreesConfig| prepare(&Prepare { sandbox: plan.as_ref(), trusted: true, ..Prepare::new(&w, cfg) });
        let notes = run(&setup_config("echo installing; echo broken >&2; exit 3", None));
        let prompt = first_prompt(&notes, "Fix the build.");
        assert!(prompt.starts_with("Note from krowk, which prepared this worktree: the worktree's setup command (`echo installing; echo broken >&2; exit 3`, from krowk's config.json) exited with code 3"), "{prompt}");
        assert!(prompt.contains("```\ninstalling\nbroken\n```") && prompt.contains(&log_of(&w).display().to_string()) && prompt.ends_with("\n\nFix the build."), "{prompt}");
        let started = Instant::now();
        let notes = run(&setup_config("echo started; sleep 5", Some(1)));
        assert!(started.elapsed() < Duration::from_secs(4), "stopped at its timeout: {:?}", started.elapsed());
        let prompt = first_prompt(&notes, "Fix the build.");
        assert!(prompt.starts_with("Note from krowk, which prepared this worktree: the worktree's setup command (`echo started; sleep 5`, from krowk's config.json) timed out after 1 s"), "{prompt}");
        assert!(prompt.contains("```\nstarted\n```"), "{prompt}");
        assert!(std::fs::read_to_string(log_of(&w)).unwrap().ends_with("(timed out after 1 s and was stopped)\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The note quotes the last 50 lines, and no more than 8 KiB of them.
    #[test]
    fn wt8_the_note_is_bounded() {
        let many: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let t = tail(&many);
        assert_eq!(t.lines().count(), NOTE_LINES);
        assert!(t.starts_with("line 50\n") && t.ends_with("line 99"), "{t}");
        let wide = "é".repeat(10_000);
        let t = tail(&wide);
        assert!(t.len() <= NOTE_BYTES + "…".len() && t.starts_with('…'), "{}", t.len());
    }
}
