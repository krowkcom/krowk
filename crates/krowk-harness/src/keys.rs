//! Stored API keys: a key kept in krowk's one credentials file
//! (`credentials.json` in krowk's home, the file SuperGrok's tokens and the
//! registry's keys are in — `0600` in the `0700` home, every write one
//! locked read-modify-write),
//! for an instance that would otherwise read it from an environment
//! variable. Before this, every key was the environment's (R-INST-5,
//! R-CRED-1): a person edited a shell's startup file, and nothing could
//! take a pasted key.
//!
//! A stored key is one of three things, as `krowk connect <vendor> --method
//! api-key` stores it:
//!
//! - **a literal** — pasted at a prompt that does not echo, or piped to
//!   `--key-stdin`; never an argument (`--key sk-…` would be in the shell's
//!   history and in every process listing);
//! - **`$VAR`** — a reference to a variable of krowk's environment, read
//!   when the registry is resolved, as a variable the definition names is;
//! - **`!command`** — a command whose standard output, trimmed, is the key
//!   (`!pass show anthropic`), run once per krowk process when it works:
//!   its key is kept for the process, two checks at once wait for one run,
//!   and a failure is theirs alone — the next call runs it again (`run`).
//!
//! **Precedence is here and nowhere else** (`apply`): a stored key, then the
//! variable. A stored key owns its instance — when it cannot be had (its
//! variable unset, its command failing, the file unreadable) the instance
//! has no key, and the error says why; krowk never falls back to the
//! environment behind it, which would send a key the person replaced.
//! `krowk disconnect` removes the stored key, and the environment is read
//! again.
//!
//! **Only the person's own file is read.** The references are honoured
//! from the credentials file alone: config.json holds a variable's *name*
//! (read, never run), a repository's `.krowk/config.json` defines no
//! instances, and the file's path is set by whoever loads the person's own
//! config (`InstancesConfig::keys_from`, which no JSON can set). The file
//! sits in krowk's home, which the file tools neither read, search nor
//! change without a person's say (`Policy::secrets`).
//!
//! **How a command runs**: `sh -c <command>` (`cmd /C` on Windows) — a shell,
//! because the person wrote the command, in their own file, to be run as
//! they would type it (`pass show x | head -n1`), and nobody else can write
//! it. It runs in the credentials file's own directory, never a
//! repository's, so a relative path or a project's configuration cannot
//! reach it. **With no terminal**, everywhere but one: stdin closed, in a
//! session of its own (`setsid`, so no controlling terminal — a prompt on
//! `/dev/tty` fails at once instead of stopping the command until its
//! timeout) and without `GPG_TTY` (so gpg-agent does not draw a curses
//! pinentry on the person's screen from under a status check), killed with
//! everything it started after `COMMAND_TIMEOUT`; the failure says to
//! unlock the password manager first or use a graphical pinentry. The one
//! exception is `krowk connect` at a terminal, where the person is there:
//! the command runs in the foreground, on that terminal, with no timeout,
//! so they can type its passphrase and the agent's cache is unlocked for
//! the runs after. Its standard output is kept to 64 KiB (more fails, the
//! command stopped); its standard error is never read into a message — a
//! failing `pass` may print what it was asked for — and neither is
//! anything it printed when it failed. What it printed is the key only
//! when it exited 0 and printed one line.
//!
//! **Readiness runs the command** (`readiness::check`, never `local`, which
//! only reads a kept key and never waits), once per process,
//! bounded like a vendor check: `krowk status` saying `ready`
//! for a key it has not seen, and the turn then failing, is what readiness
//! exists to prevent, and the command is one the person gave krowk to run.
//! It runs in parallel with the vendor checks, and a turn in the same
//! process reuses the key. A command that fails is `unknown`, with why.
//!
//! No key reaches a log, an error, `{:?}` or `krowk status`: status shows
//! where it comes from — `stored`, `stored ($VAR)`, `stored (!pass …)`, the
//! command cut to its program — and never the value.

use crate::engine::EngineError;
use crate::instances::{Auth, Resolved};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// How long a key's command may run with no terminal: it cannot ask for a
/// passphrase there, so this bounds a slow password manager or a network
/// fetch, not a person typing. At a terminal (`krowk connect`) there is no
/// limit: the person is there to stop it.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A stored key, as the credentials file holds it (`keys` in it, by
/// instance): `{"literal": "sk-…"}`, `{"env": "VAR"}`, `{"command": "…"}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyRef {
    Literal(String),
    Env(String),
    Command(String),
}

// Hand-written so a key never reaches a log line through `{:?}`.
impl std::fmt::Debug for KeyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source())
    }
}

impl KeyRef {
    /// What a person typed — at the prompt, or to `--key-ref`: `$VAR` or
    /// `${VAR}`, `!command`, else a literal key. A key is one line of
    /// printable text.
    pub fn parse(s: &str) -> Result<KeyRef, String> {
        let s = s.trim();
        if let Some(var) = s.strip_prefix('$') {
            let var = var.strip_prefix('{').and_then(|v| v.strip_suffix('}')).unwrap_or(var);
            let ok = var.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            return if ok { Ok(KeyRef::Env(var.into())) } else { Err(format!("`${var}` is not a variable's name — letters, digits and `_`, e.g. $ANTHROPIC_API_KEY")) };
        }
        if let Some(cmd) = s.strip_prefix('!') {
            return match cmd.trim() {
                "" => Err("`!` needs the command after it, e.g. !pass show anthropic".into()),
                c => Ok(KeyRef::Command(c.into())),
            };
        }
        KeyRef::literal(s)
    }

    /// A key itself — as piped to `--key-stdin`, whatever it starts with.
    pub fn literal(s: &str) -> Result<KeyRef, String> {
        match s.trim() {
            "" => Err("the key is empty".into()),
            k if k.chars().any(char::is_control) => Err("a key is one line, with no control characters".into()),
            k => Ok(KeyRef::Literal(k.into())),
        }
    }

    /// Where the key comes from, as `krowk status` says it: never the key,
    /// and of a command only its program.
    pub fn source(&self) -> String {
        match self {
            KeyRef::Literal(_) => "stored".into(),
            KeyRef::Env(v) => format!("stored (${v})"),
            KeyRef::Command(c) => format!("stored (!{})", short(c)),
        }
    }
}

/// A command as it may be shown: its program, then `…` when there is more
/// — the rest may be an entry's name, or a key written inline. Leading
/// `NAME=value` words (`KEY=sk-… pass …`) are skipped, and a program that
/// still holds a `=`, or none at all, is shown as `…` alone.
pub fn short(command: &str) -> String {
    let assign = |w: &str| w.split_once('=').is_some_and(|(n, _)| n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    let mut words = command.split_whitespace().skip_while(|w| assign(w));
    match words.next() {
        Some(p) if !p.contains('=') => {
            let p: String = p.chars().filter(|c| !c.is_control()).collect();
            if words.next().is_some() { format!("{p} …") } else { p }
        }
        _ => "…".into(),
    }
}

/// How a resolved instance's key is stored — the reference, never the key.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Stored {
    /// None: the environment variable the definition names.
    #[default]
    No,
    Literal,
    Env(String),
    /// Run in `dir`, the credentials file's own directory.
    Command { command: String, dir: PathBuf },
    /// The credentials file could not be read, so whether a key is stored
    /// is not known: no key, rather than the environment's.
    Unreadable(String),
}

// A command may hold a key written into it: only its program is shown.
impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source().unwrap_or_else(|| "not stored".into()))
    }
}

impl Stored {
    /// `KeyRef::source`'s words, for a resolved instance.
    pub fn source(&self) -> Option<String> {
        Some(match self {
            Stored::No => return None,
            Stored::Literal | Stored::Unreadable(_) => "stored".into(),
            Stored::Env(v) => format!("stored (${v})"),
            Stored::Command { command, .. } => format!("stored (!{})", short(command)),
        })
    }
}

/// The one place precedence is decided: every instance that calls an API
/// with a key (not a backend, not OAuth) takes its stored key when the
/// credentials file has one — a literal or a variable read now, a command
/// left to `materialise` — and a compatible server with one is keyed.
pub(crate) fn apply(instances: &mut BTreeMap<String, Resolved>, credentials: &Path, env: &dyn Fn(&str) -> String) {
    let stored = crate::oauth::Store::new(credentials.to_path_buf()).keys();
    let dir = credentials.parent().unwrap_or(Path::new(".")).to_path_buf();
    for r in instances.values_mut().filter(|r| r.backend.is_none() && matches!(r.auth, Auth::ApiKey | Auth::Keyless)) {
        let k = match &stored {
            Ok(all) => match all.get(&r.name) {
                Some(k) => k,
                None => continue,
            },
            Err(e) if r.auth == Auth::ApiKey => {
                (r.stored, r.api_key) = (Stored::Unreadable(e.message.clone()), String::new());
                continue;
            }
            Err(_) => continue,
        };
        r.auth = Auth::ApiKey;
        (r.stored, r.api_key) = match k {
            KeyRef::Literal(v) => (Stored::Literal, v.trim().to_string()),
            KeyRef::Env(v) => (Stored::Env(v.clone()), env(v).trim().to_string()),
            KeyRef::Command(c) => (Stored::Command { command: c.clone(), dir: dir.clone() }, ran(c).unwrap_or_default()),
        };
    }
}

/// What each command gave in this process, by command. Each has its own
/// lock, held while the command runs, so two checks at once (status's
/// parallel pass, the TUI routing while its first turn starts) wait for one
/// run — one passphrase prompt. A key is kept for the process. A failure
/// is shared only with the calls that were waiting on that run, and never
/// kept: the person told to unlock their password manager does so and
/// tries again, and a remembered failure would refuse every later turn of
/// a long-lived host (the TUI) for nothing — as readiness never remembers
/// "not signed in".
#[derive(Default)]
struct Memo {
    /// Runs finished: a waiter that saw fewer before it waited takes the
    /// answer of the run it waited on, even a failure.
    runs: std::sync::atomic::AtomicU64,
    last: Mutex<Option<Result<String, String>>>,
}
static RUNS: Mutex<Option<HashMap<String, std::sync::Arc<Memo>>>> = Mutex::new(None);

fn memo(command: &str) -> std::sync::Arc<Memo> {
    RUNS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry(command.into()).or_default().clone()
}

/// The key a command already gave in this process — never waiting: a
/// command still running, or one that failed, is `None`, left to
/// `readiness::check`, which may block.
pub fn ran(command: &str) -> Option<String> {
    let m = RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(command)?.clone();
    let last = m.last.try_lock().ok()?;
    last.as_ref().and_then(|r| r.as_ref().ok().cloned())
}

/// Forgets every command's answer: a connection or a disconnection has just
/// changed what the person wants run.
pub fn forget() {
    RUNS.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// The instance with its key in hand: a command's run (once per process),
/// or why there is none. Everything else is as it was resolved.
pub fn materialise(inst: &Resolved) -> Result<Cow<'_, Resolved>, EngineError> {
    match &inst.stored {
        Stored::Unreadable(why) => Err(EngineError::new("not_authenticated", format!("{why} — so whether {} has a stored key is not known, and krowk does not fall back to ${} for it", inst.name, inst.api_key_env))),
        Stored::Command { command, dir } if inst.api_key.is_empty() => {
            let key = run(&inst.name, command, dir, None).map_err(|why| EngineError::new("not_authenticated", no_fallback(inst, &why)))?;
            let mut r = inst.clone();
            r.api_key = key;
            Ok(Cow::Owned(r))
        }
        _ => Ok(Cow::Borrowed(inst)),
    }
}

/// What to check when a provider refuses an instance's key: the stored
/// one, where there is one, else the variable.
pub fn auth_fix(inst: &Resolved) -> String {
    match inst.stored.source() {
        Some(s) => format!("check its key ({s} in krowk's provider credentials file), or store another with `{}`", crate::connect::connect_command(&inst.name, inst.kind)),
        None if inst.api_key_env.is_empty() => "the server wants a key: give the instance an apiKeyEnv".into(),
        None => format!("check {}", inst.api_key_env),
    }
}

/// A stored key that cannot be had, said with what is not done about it.
fn no_fallback(inst: &Resolved, why: &str) -> String {
    let var = if inst.api_key_env.is_empty() { String::new() } else { format!(" to ${}", inst.api_key_env) };
    format!("{why} — the stored key is {}'s own, so krowk does not fall back{var}; fix it, store another with `{}`, or `krowk disconnect {}` to read the environment again", inst.name, crate::connect::connect_command(&inst.name, inst.kind), inst.name)
}

/// Runs a key's command (see the module's notes), once per process: a
/// key already had, or the run another thread is making, is its answer
/// (see `Memo`: a failure only for the calls that waited on it). The
/// error never holds anything it printed.
///
/// `at` is a person's terminal to run it on (`krowk connect` at one): in
/// the foreground there, its prompts and errors theirs to see, so a
/// password manager can ask for its passphrase — and a run made there is
/// made again, whatever was remembered. Otherwise it runs with no terminal
/// at all (its own session, `GPG_TTY` taken away), so one that asks fails
/// at once instead of drawing a prompt nobody can answer.
pub fn run(instance: &str, command: &str, dir: &Path, at: Option<&mut dyn crate::connect::AuthInteraction>) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    let m = memo(command);
    let seen = m.runs.load(Ordering::SeqCst);
    let mut last = m.last.lock().unwrap_or_else(|e| e.into_inner());
    match last.as_ref() {
        Some(Ok(k)) if at.is_none() => return Ok(k.clone()),
        // Failed while this call waited: that run's answer is this one's.
        Some(Err(e)) if at.is_none() && m.runs.load(Ordering::SeqCst) > seen => return Err(e.clone()),
        _ => {}
    }
    let r = run_once(instance, command, dir, at);
    *last = Some(r.clone());
    m.runs.fetch_add(1, Ordering::SeqCst);
    r
}

fn run_once(instance: &str, command: &str, dir: &Path, at: Option<&mut dyn crate::connect::AuthInteraction>) -> Result<String, String> {
    let what = format!("{instance}'s stored key comes from running `{}`, which", short(command));
    #[cfg(unix)]
    let mut cmd = std::process::Command::new("sh");
    #[cfg(unix)]
    cmd.arg("-c").arg(command);
    #[cfg(not(unix))]
    let mut cmd = std::process::Command::new("cmd");
    #[cfg(not(unix))]
    cmd.arg("/C").arg(command);
    cmd.current_dir(dir);
    let (status, stdout) = match at {
        Some(ui) => {
            let mut out = Vec::new();
            let status = ui.terminal(&mut || {
                use std::io::Read;
                let mut child = cmd.stdin(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
                let read = child.stdout.take().map(|o| o.take(crate::readiness::OUTPUT_CAP as u64 + 1).read_to_end(&mut out));
                if read.is_some_and(|r| r.is_err()) || out.len() > crate::readiness::OUTPUT_CAP {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("it printed more than {} KiB", crate::readiness::OUTPUT_CAP / 1024));
                }
                child.wait().map_err(|e| e.to_string())
            })
            .map_err(|e| format!("{what} failed: {e}"))?;
            (status, out)
        }
        None => {
            let probe = crate::readiness::Probe { dir: dir.to_path_buf(), within: COMMAND_TIMEOUT };
            cmd.env_remove("GPG_TTY");
            match crate::readiness::output_detached(&mut cmd, &probe) {
                Err(e) => return Err(format!("{what} failed: {e}")),
                Ok(None) => return Err(format!("{what} did not finish within {} seconds{UNLOCK}", COMMAND_TIMEOUT.as_secs())),
                Ok(Some(o)) => (o.status, o.stdout),
            }
        }
    };
    if !status.success() {
        return Err(format!("{what} stopped ({status}) — what it printed is not shown, since it may hold the key; run it yourself to see why{UNLOCK}"));
    }
    let key = String::from_utf8(stdout).map_err(|_| format!("{what} printed something that is not text"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("{what} printed nothing"));
    }
    if key.chars().any(char::is_control) {
        return Err(format!("{what} printed more than one line — a key is one line; keep the first with `… | head -n1`"));
    }
    Ok(key.into())
}

/// Said of a command that failed with no terminal: the likeliest reason.
const UNLOCK: &str = ". krowk runs it with no terminal, so if it asks for a passphrase it cannot — unlock your password manager first (run the command once yourself), or use a graphical pinentry";

#[cfg(test)]
mod tests {
    use super::*;

    // A command that fails (a locked password manager) is not remembered:
    // unlocked, the next call in the same process (a TUI's next turn) runs
    // it again and has the key, which is then kept.
    #[cfg(unix)]
    #[test]
    fn r_cred_1_a_failed_command_is_run_again_and_a_key_is_kept() {
        let d = std::env::temp_dir().join(format!("krowk-keys-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (locked, runs) = (d.join("locked"), d.join("runs"));
        std::fs::write(&locked, "").unwrap();
        let cmd = format!("echo run >> {}; [ -e {} ] && exit 1; echo sk-unlocked", runs.display(), locked.display());
        assert!(run("anthropic", &cmd, &d, None).unwrap_err().contains("stopped"));
        assert_eq!(ran(&cmd), None, "a failure is not kept");
        std::fs::remove_file(&locked).unwrap();
        assert_eq!(run("anthropic", &cmd, &d, None).unwrap(), "sk-unlocked", "fixed, the next call has the key");
        assert_eq!(run("anthropic", &cmd, &d, None).unwrap(), "sk-unlocked");
        assert_eq!((ran(&cmd).as_deref(), std::fs::read_to_string(&runs).unwrap().lines().count()), (Some("sk-unlocked"), 2), "the key is kept: no third run");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn r_cred_1_a_reference_is_parsed_and_never_shown_with_its_key() {
        assert_eq!(KeyRef::parse(" $ANTHROPIC_KEY "), Ok(KeyRef::Env("ANTHROPIC_KEY".into())));
        assert_eq!(KeyRef::parse("${WORK_KEY}"), Ok(KeyRef::Env("WORK_KEY".into())));
        assert_eq!(KeyRef::parse("!pass show anthropic"), Ok(KeyRef::Command("pass show anthropic".into())));
        assert_eq!(KeyRef::parse("sk-ant-SENTINEL"), Ok(KeyRef::Literal("sk-ant-SENTINEL".into())));
        for bad in ["$", "$1X", "$A-B", "!", "", "sk\nx"] {
            assert!(KeyRef::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(format!("{:?}", KeyRef::Literal("sk-ant-SENTINEL".into())), "stored");
        assert_eq!(KeyRef::Command("pass show anthropic".into()).source(), "stored (!pass …)");
        assert_eq!(KeyRef::Command("echo sk-inline".into()).source(), "stored (!echo …)", "a key written into the command stays out");
        assert_eq!(KeyRef::Env("K".into()).source(), "stored ($K)");
        for (cmd, shown) in [("FOO=sk-x pass show a", "pass …"), ("KEY=sk-x", "…"), ("a=b", "…"), ("pass", "pass"), ("--key=sk-x x", "…")] {
            assert_eq!(short(cmd), shown, "{cmd}");
        }
    }
}
