//! Stored API keys: a key kept in krowk's provider credentials file
//! (`providers/credentials.json`, the file SuperGrok's tokens are in —
//! `0600` in a `0700` directory, every write one locked read-modify-write),
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
//!   (`!pass show anthropic`), run at most once per krowk process.
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
//! sits in krowk's config directory, which the file tools change only with
//! a person's say, and the file tools neither read it nor search through it
//! unasked (`Policy::secrets`).
//!
//! **How a command runs**: `sh -c <command>` (`cmd /C` on Windows) — a shell,
//! because the person wrote the command, in their own file, to be run as
//! they would type it (`pass show x | head -n1`), and nobody else can write
//! it. It runs in the credentials file's own directory, never a
//! repository's, so a relative path or a project's configuration cannot
//! reach it; with stdin closed; in a process group of its own, killed with
//! everything it started after `COMMAND_TIMEOUT`. Its standard error is
//! never read into a message — a failing `pass` may print what it was
//! asked for — and neither is anything it printed when it failed. What it
//! printed is the key only when it exited 0 and printed one line.
//!
//! **Readiness runs the command** (`readiness::check`, never `local`), once
//! per process, bounded like a vendor check: `krowk status` saying `ready`
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

/// How long a key's command may run: long enough for a password manager
/// to ask for its passphrase.
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

/// A command as it may be shown: its first word, then `…` when there is
/// more — the rest may be an entry's name, or a key written inline.
pub fn short(command: &str) -> String {
    let mut words = command.split_whitespace();
    let first: String = words.next().unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
    if words.next().is_some() { format!("{first} …") } else { first }
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

/// Keys commands printed, by command, for this process.
static RAN: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The key a command already printed in this process.
pub fn ran(command: &str) -> Option<String> {
    RAN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(command).cloned())
}

/// The instance with its key in hand: a command's run (once per process),
/// or why there is none. Everything else is as it was resolved.
pub fn materialise(inst: &Resolved) -> Result<Cow<'_, Resolved>, EngineError> {
    match &inst.stored {
        Stored::Unreadable(why) => Err(EngineError::new("not_authenticated", format!("{why} — so whether {} has a stored key is not known, and krowk does not fall back to ${} for it", inst.name, inst.api_key_env))),
        Stored::Command { command, dir } if inst.api_key.is_empty() => {
            let key = run(&inst.name, command, dir).map_err(|why| EngineError::new("not_authenticated", no_fallback(inst, &why)))?;
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

/// Runs a key's command (see the module's notes) and keeps what it printed
/// for this process. The error never holds anything it printed.
pub fn run(instance: &str, command: &str, dir: &Path) -> Result<String, String> {
    if let Some(k) = ran(command) {
        return Ok(k);
    }
    let what = format!("{instance}'s stored key comes from running `{}`, which", short(command));
    #[cfg(unix)]
    let mut cmd = std::process::Command::new("sh");
    #[cfg(unix)]
    cmd.arg("-c").arg(command);
    #[cfg(not(unix))]
    let mut cmd = std::process::Command::new("cmd");
    #[cfg(not(unix))]
    cmd.arg("/C").arg(command);
    let probe = crate::readiness::Probe { dir: dir.to_path_buf(), within: COMMAND_TIMEOUT };
    let out = match crate::readiness::output_within(&mut cmd, &probe) {
        Err(e) => return Err(format!("{what} could not start: {e}")),
        Ok(None) => return Err(format!("{what} did not finish within {} seconds", COMMAND_TIMEOUT.as_secs())),
        Ok(Some(o)) => o,
    };
    if !out.status.success() {
        return Err(format!("{what} stopped ({}) — what it printed is not shown, since it may hold the key; run it yourself to see why", out.status));
    }
    let key = String::from_utf8(out.stdout).map_err(|_| format!("{what} printed something that is not text"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("{what} printed nothing"));
    }
    if key.chars().any(char::is_control) {
        return Err(format!("{what} printed more than one line — a key is one line; keep the first with `… | head -n1`"));
    }
    RAN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(command.into(), key.into());
    Ok(key.into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
