//! A Claude Code instance's login, asked of Claude Code (R-INST-2). krowk
//! never runs a Claude OAuth flow and never reads Claude's credentials — the
//! keychain entry or `.credentials.json` (R-BACK-2): `claude auth login`
//! signs in, in Anthropic's own flow on the person's own terminal, and
//! `claude auth status` says whether it did. Both run with the instance's
//! `CLAUDE_CONFIG_DIR`, so each instance is its own account.

use crate::instances::Backend;
use serde_json::Value;
use std::process::{Command, ExitStatus, Stdio};

/// What `claude auth status` reports, less anything personal: the email it
/// prints is never read into krowk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub logged_in: bool,
    /// `claude.ai` for a subscription login, `console` or an API key otherwise.
    pub auth_method: String,
    /// The subscription, when the login is one: `pro`, `max`, `team` …
    pub subscription: String,
}

impl Status {
    /// Plain words for a listing (R-BACK-2: plain text, no branding).
    pub fn describe(&self) -> String {
        match (self.logged_in, self.auth_method.as_str(), self.subscription.as_str()) {
            (false, _, _) => "not signed in".into(),
            (true, "claude.ai", "") => "signed in with a Claude subscription".into(),
            (true, "claude.ai", s) => format!("signed in with a Claude {s} subscription"),
            (true, m, _) if !m.is_empty() => format!("signed in ({m})"),
            _ => "signed in".into(),
        }
    }
}

fn command(b: &Backend, args: &[&str]) -> Command {
    let mut c = Command::new(b.path.as_deref().unwrap_or(std::path::Path::new(&b.binary)));
    c.args(args);
    let (remove, set) = crate::claude::environment(b);
    for k in remove {
        c.env_remove(k);
    }
    c.envs(set);
    c
}

fn not_found(b: &Backend, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("{} was not found — install Claude Code, or name the binary with --binary", b.binary)
    } else {
        format!("{} could not be started: {e}", b.binary)
    }
}

/// `claude auth status`, read as JSON. Signed out is an answer (Claude Code
/// exits 1 for it), not a failure; no answer is — nor one that takes longer
/// than the readiness check waits for any vendor.
/// It runs where `probe` says: Claude Code reads the project settings of
/// its working directory, and they can decide the answer.
pub fn status(b: &Backend, probe: &crate::readiness::Probe) -> Result<Status, String> {
    let out = crate::readiness::output_within(&mut command(b, &["auth", "status", "--json"]), probe)
        .map_err(|e| not_found(b, e))?
        .ok_or_else(|| format!("`{} auth status` did not answer within {} seconds", b.binary, probe.within.as_secs_f32()))?;
    let v: Value = serde_json::from_slice(out.stdout.trim_ascii()).map_err(|_| format!("`{} auth status --json` did not answer in JSON — is it Claude Code?", b.binary))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Status { logged_in: v.get("loggedIn").and_then(Value::as_bool).unwrap_or(false), auth_method: s("authMethod"), subscription: s("subscriptionType") })
}

/// `claude auth login`, on the person's own terminal: its prompts, its
/// browser, its answer. krowk sees only how it exited. What it prints goes
/// to stderr, the terminal either way, so krowk's own answer on stdout stays
/// one JSON document.
pub fn login(b: &Backend) -> Result<ExitStatus, String> {
    command(b, &["auth", "login"]).stdin(Stdio::inherit()).stdout(Stdio::from(std::io::stderr())).stderr(Stdio::inherit()).status().map_err(|e| not_found(b, e))
}
