//! A Codex login, as Codex reports it (R-INST-2, with `codex login` in
//! place of `claude auth login`). krowk never opens Codex's login file or
//! runs an OAuth flow of its own with Codex's client: it asks `codex login
//! status`, and a login is made by `codex login` itself, on the person's
//! own terminal, in OpenAI's own flow. Both run with the instance's
//! `CODEX_HOME`, so each account signs in, and is asked about, on its own.
//! Whether there is a login is asked first of `codex app-server`'s
//! `account/read` (`account`), a structured answer, and of `codex login
//! status`'s words only when that fails.

use crate::instances::Backend;
use crate::protocol::Billing;
use crate::readiness::Probe;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

/// What `codex login status` said. Only whether there is a login and what
/// it is billed to are kept: an API-key login prints part of its key, and
/// that is never read past the words that name the method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub logged_in: bool,
    pub billing: Option<Billing>,
}

impl Status {
    /// For a listing: how the instance is signed in.
    pub fn describe(&self) -> String {
        match (self.logged_in, self.billing) {
            (false, _) => "not signed in".into(),
            (true, Some(Billing::Subscription)) => "signed in with ChatGPT".into(),
            (true, Some(Billing::ApiKey)) => "signed in with an API key".into(),
            (true, None) => "signed in".into(),
        }
    }

    /// Reads `codex login status`: it exits 0 with `Logged in using …`
    /// when there is a login, and non-zero with `Not logged in` when there
    /// is none, on stderr or stdout depending on the version.
    pub fn parse(success: bool, said: &str) -> Status {
        let line = said.lines().map(str::trim).find(|l| l.starts_with("Logged in") || l.starts_with("Not logged in")).unwrap_or_default();
        let logged_in = success && line.starts_with("Logged in");
        let method = line.split(" - ").next().unwrap_or_default().to_ascii_lowercase();
        let billing = logged_in
            .then(|| {
                if method.contains("chatgpt") {
                    Some(Billing::Subscription)
                } else if method.contains("api key") {
                    Some(Billing::ApiKey)
                } else {
                    None
                }
            })
            .flatten();
        Status { logged_in, billing }
    }
}

/// `codex <args>` for this instance: its binary, its `CODEX_HOME`, and the
/// environment a backend process gets.
pub fn command(b: &Backend, args: &[&str]) -> Command {
    let mut c = Command::new(b.path.as_deref().unwrap_or(Path::new(&b.binary)));
    c.args(args);
    let (remove, set) = super::environment(b);
    for k in remove {
        c.env_remove(k);
    }
    c.envs(set);
    c
}

/// Asks Codex whether this instance is signed in, as `codex login status`
/// words it: the fallback for a Codex whose app-server cannot be asked.
pub fn status(b: &Backend, probe: &Probe) -> Result<Status, String> {
    let out = crate::readiness::output_within(&mut command(b, &["login", "status"]), probe)
        .map_err(|e| format!("{} could not be run: {e}", b.binary))?
        .ok_or_else(|| format!("`{} login status` did not answer within {} seconds", b.binary, probe.within.as_secs_f32()))?;
    let said = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    if !said.contains("Logged in") && !said.contains("Not logged in") {
        return Err(format!("`{} login status` answered in a way krowk does not read (exit {})", b.binary, out.status));
    }
    Ok(Status::parse(out.status.success(), &said))
}

/// Asks Codex whether this instance is signed in, and to what, the way the
/// backend itself asks before a turn: `initialize`, then `account/read`, of
/// a `codex app-server` started for nothing else and stopped at once. A
/// structured answer, where `codex login status` is words that have
/// changed between versions. Only the account's type is read — never its
/// email or plan's owner.
///
/// No account, where Codex says it needs none (`requiresOpenaiAuth` false —
/// a model provider of its own config that takes no OpenAI login), is as
/// good as signed in: a turn runs.
pub fn account(b: &Backend, probe: &Probe) -> Result<Status, String> {
    let within = probe.within;
    use std::io::{BufRead, Write};
    // The account's links to the person's configuration, as before any
    // app-server the backend starts: without them a new `skills` would be
    // Codex's to write into through a stale link.
    if let (Some(home), Some(own)) = (&b.config_dir, &b.shared_home) {
        let _ = super::share(home, own);
    }
    // Where the probe says, in a group of its own, as every check runs
    // (`readiness::probing`): a project's `.codex/config.toml` can change
    // whether a login is needed.
    let mut cmd = command(b, &super::args(&b.args).iter().map(String::as_str).collect::<Vec<_>>());
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = crate::readiness::probing(&mut cmd, probe).map_err(|e| format!("{} could not be run: {e}", b.binary))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let out = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let asked = child.stdin.take().map(|mut i| {
        let lines = [
            json!({"id": 1, "method": "initialize", "params": super::initialize_params(env!("CARGO_PKG_VERSION"))}),
            json!({"method": "initialized"}),
            json!({"id": 2, "method": "account/read", "params": {"refreshToken": false}}),
        ];
        let sent = lines.iter().try_for_each(|l| writeln!(i, "{l}"));
        (i, sent)
    });
    let deadline = Instant::now() + within;
    let answer = match asked {
        Some((_, Err(e))) => Err(format!("`{} app-server` did not take the question: {e}", b.binary)),
        None => Err(format!("`{} app-server` has no stdin", b.binary)),
        Some((_stdin, Ok(()))) => loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if v.get("id").and_then(Value::as_u64) != Some(2) || v.get("method").is_some() {
                        continue;
                    }
                    break match v.get("result") {
                        Some(r) => Ok(read_account(r)),
                        None => Err(format!("`{} app-server` refused account/read", b.binary)),
                    };
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break Err(format!("`{} app-server` did not answer account/read within {} seconds", b.binary, within.as_secs_f32())),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(format!("`{} app-server` exited before it answered account/read", b.binary)),
            }
        },
    };
    crate::readiness::stop(&mut child);
    answer
}

/// Whether this instance is signed in: `account/read` (`account`) first,
/// `codex login status`'s words (`status`) only when app-server cannot be
/// asked — an older Codex, one that will not start it. What each failed on
/// is the error when both do.
///
/// Both share the probe's one deadline: the fallback gets what is left, so
/// the whole check is over by then, as for any vendor.
pub fn signed_in(b: &Backend, probe: &Probe) -> Result<Status, String> {
    let started = Instant::now();
    account(b, probe).or_else(|structured| {
        let left = probe.within.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(structured);
        }
        status(b, &Probe { within: left, ..probe.clone() }).map_err(|text| format!("{structured}; {text}"))
    })
}

/// `account/read`'s result: a ChatGPT account is a subscription, any other
/// an API key; none is signed out unless Codex needs no login at all.
fn read_account(r: &Value) -> Status {
    match r.pointer("/account/type").and_then(Value::as_str) {
        Some("chatgpt") => Status { logged_in: true, billing: Some(Billing::Subscription) },
        Some(_) => Status { logged_in: true, billing: Some(Billing::ApiKey) },
        None => Status { logged_in: r.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(false), billing: None },
    }
}

/// Runs `codex login` on this terminal: OpenAI's own sign-in, which opens
/// a browser or prints a link, and writes the login into the instance's
/// home. `device` asks Codex for its device-code flow instead. What Codex
/// prints goes to stderr, where the person reads it, so krowk's own stdout
/// stays the one answer a script parses.
pub fn login(b: &Backend, device: bool) -> Result<ExitStatus, String> {
    let args: &[&str] = if device { &["login", "--device-auth"] } else { &["login"] };
    command(b, args).stdin(Stdio::inherit()).stdout(std::io::stderr()).stderr(Stdio::inherit()).status().map_err(|e| format!("{} could not be run: {e}", b.binary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_inst_2_codex_login_status_is_read_for_its_method_and_nothing_else() {
        let chatgpt = Status::parse(true, "Logged in using ChatGPT\n");
        assert_eq!(chatgpt, Status { logged_in: true, billing: Some(Billing::Subscription) });
        assert_eq!(chatgpt.describe(), "signed in with ChatGPT");
        let key = Status::parse(true, "WARNING: something\nLogged in using an API key - sk-proj-***ABCD\n");
        assert_eq!(key.billing, Some(Billing::ApiKey));
        assert!(!format!("{key:?}").contains("ABCD"), "the key's shown part is not kept");
        assert_eq!(Status::parse(false, "Not logged in\n"), Status { logged_in: false, billing: None });
        assert_eq!(Status::parse(false, "Logged in using ChatGPT"), Status { logged_in: false, billing: None }, "the exit code decides");
    }
}
