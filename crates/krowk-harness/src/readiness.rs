//! Whether an instance can run a turn here, asked one way everywhere:
//! `krowk status`, `krowk doctor`, `krowk providers list`, and the host
//! before it moves a session or starts a turn (`check_model`, `settle`,
//! rollover candidates). Before this, `providers list` spawned each vendor
//! CLI in turn, the host checked only keys and binaries, and a SuperGrok
//! login counted as there whenever its name was in the credentials file.
//!
//! The answer is a `Readiness` and, around it, a `Report`: where the
//! credential comes from (`source` — an environment variable's name, a
//! vendor's own login, krowk's OAuth file; never the secret) and one line
//! that fixes it. A key is checked in the environment krowk resolved at
//! start, or — stored — in krowk's provider credentials file, whose
//! `!command` is the one command `check` runs besides a vendor's (see
//! `crate::keys`), an OAuth login in krowk's provider credentials file (an expired
//! access token with no refresh token is `Expired`: nothing krowk can do
//! renews it), and a vendor login by asking the vendor — `claude auth status
//! --json`, Codex's `account/read` over `codex app-server` with `codex login
//! status` as the fallback — never by reading its files (R-BACK-2, R-BACK-3).
//!
//! **Where a vendor is asked matters.** Claude Code reads a project's
//! `.claude/settings.json` from its working directory, and a project can
//! make the login moot there (Bedrock or Vertex in its `env`, an
//! `apiKeyHelper`); Codex reads a project's `.codex/config.toml` the same
//! way (a `model_provider` that needs no OpenAI login). So a `Probe` names
//! the directory: before a turn, the session's own working directory,
//! once its repository is trusted — where the turn starts the vendor, and
//! so the answer the turn will get (Claude Code reads the settings of that
//! directory alone, not its parents') — and for `krowk status`, `providers
//! list`, `doctor` and a rollover offer, where no repository has been
//! trusted, krowk's own `0700` directory (`neutral_dir`), which holds
//! nothing and which nobody else can write into — never the shared
//! temporary directory, where anyone could plant a `.claude/settings.json`.
//!
//! Vendor checks spawn a process, so `check_all` runs them in parallel,
//! each bounded by one deadline (`VENDOR_TIMEOUT`) and killed with its
//! whole process group when it passes (unix; on Windows only the process
//! krowk started, see `probing`), and a long-lived host (the TUI) does
//! not re-ask for every switch: a vendor's "signed in" is kept for
//! `CACHE_FOR`. Only "signed in" is kept. A person who is told to sign in
//! does so in another terminal and tries again at once, and a remembered
//! "not signed in" would refuse them for a minute for nothing.

use crate::claude::auth as claude_auth;
use crate::codex::auth as codex_auth;
use crate::connect;
use crate::engine::EngineError;
use crate::catalog::Listed;
use crate::instances::{kind_label, Asked, Auth, Backend, Registry, Resolved};
use crate::keys::{self, Stored};
use crate::oauth;
use crate::protocol::{ModelRef, WireApi};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one vendor check may take before its answer is `Unknown`. Long
/// enough for a cold `claude` (a Node start) on a slow disk; checks run in
/// parallel, so a listing waits for the slowest one, not their sum.
pub const VENDOR_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a vendor's "signed in" is believed without asking again.
pub const CACHE_FOR: Duration = Duration::from_secs(60);
/// The most of a check's output that is kept, per stream (see
/// `output_within`).
pub const OUTPUT_CAP: usize = 64 * 1024;

/// Where a vendor is asked, and how long it has to answer — one deadline
/// for the whole check, a fallback included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub dir: PathBuf,
    pub within: Duration,
}

impl Probe {
    /// In `dir`, with `VENDOR_TIMEOUT`.
    pub fn at(dir: impl Into<PathBuf>) -> Probe {
        Probe { dir: dir.into(), within: VENDOR_TIMEOUT }
    }
}

/// krowk's own directory for asking a vendor outside any repository:
/// `readiness/` in krowk's home, made `0700` and kept that way, and refused
/// when it is anything but a directory of its own (a symlink planted there
/// would lead the check somewhere else) — the home's own rules
/// (`krowk_api::home::own`). It holds nothing, so a vendor started in it
/// reads only the person's own settings.
pub fn neutral_dir(home: &Path) -> Result<PathBuf, String> {
    let dir = home.join(krowk_api::home::READINESS);
    krowk_api::home::make(&dir)?;
    Ok(dir)
}

/// Whether an instance can run a turn, and if not, which kind of not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// It can. `source` names where the credential comes from.
    Ready { source: String },
    /// An API key is read from `var`, and `var` is not set.
    KeyNotSet { var: String },
    /// No login: the vendor says so, or krowk's OAuth file has none.
    NotSignedIn,
    /// A login whose access token has expired and cannot be refreshed.
    Expired,
    /// The vendor binary is not there.
    NotInstalled,
    /// The check itself failed: a vendor that did not answer in time, or
    /// answered in a way krowk does not read. Not a refusal: the turn is
    /// left to fail, or not, on its own.
    Unknown { reason: String },
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Readiness::Ready { .. })
    }

    /// The `state` field of `krowk status --json`: stable, snake_case.
    pub fn state(&self) -> &'static str {
        match self {
            Readiness::Ready { .. } => "ready",
            Readiness::KeyNotSet { .. } => "key_not_set",
            Readiness::NotSignedIn => "not_signed_in",
            Readiness::Expired => "expired",
            Readiness::NotInstalled => "not_installed",
            Readiness::Unknown { .. } => "unknown",
        }
    }

    /// The same, in words, for a table.
    pub fn label(&self) -> &'static str {
        match self {
            Readiness::Ready { .. } => "ready",
            Readiness::KeyNotSet { .. } => "key not set",
            Readiness::NotSignedIn => "not signed in",
            Readiness::Expired => "expired",
            Readiness::NotInstalled => "not installed",
            Readiness::Unknown { .. } => "unknown",
        }
    }
}

/// One instance's row: what `krowk status` prints, and what a refusal is
/// made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub instance: String,
    pub kind: &'static str,
    pub readiness: Readiness,
    /// Where the credential comes from — or, when it is not there, would.
    /// A name or a place, never a secret.
    pub source: String,
    /// What makes it ready; none when it is.
    pub fix: Option<String>,
}

impl Report {
    /// A row of `krowk status --json` (and `providers list`, `doctor`).
    /// Every key is always there, `null` when it has no value, so a script
    /// reads the same shape whatever the state.
    pub fn json(&self) -> Value {
        json!({
            "instance": self.instance,
            "kind": self.kind,
            "label": crate::instances::kind_label(self.kind),
            "state": self.readiness.state(),
            "ready": self.readiness.is_ready(),
            "source": self.source,
            "fix": self.fix,
            "var": match &self.readiness { Readiness::KeyNotSet { var } => Some(var), _ => None },
            "reason": match &self.readiness { Readiness::Unknown { reason } => Some(reason), _ => None },
        })
    }

    /// Why a turn cannot run here, with its fix, before a session is moved
    /// or a process started — or none: ready, or a check that could not
    /// tell, which is left to the turn.
    pub fn refusal(&self, inst: &Resolved) -> Option<EngineError> {
        let name = &self.instance;
        let fix = self.fix.clone().unwrap_or_default();
        Some(match &self.readiness {
            Readiness::Ready { .. } | Readiness::Unknown { .. } => return None,
            Readiness::KeyNotSet { .. } => EngineError::new("not_authenticated", inst.missing_key().unwrap_or(fix)),
            Readiness::NotInstalled => EngineError::new("backend_not_found", format!("{} was not found — {fix}", inst.backend.as_ref().map(|b| b.binary.as_str()).unwrap_or_default())),
            Readiness::NotSignedIn => match inst.auth {
                Auth::OAuth { .. } => EngineError::new("not_authenticated", format!("the {name} instance is not signed in — {fix} (a SuperGrok or X Premium subscription)")),
                _ => EngineError::new("not_authenticated", format!("{} is not signed in for the {name} instance — {fix}", inst.vendor)).with_status(401),
            },
            Readiness::Expired => EngineError::new("not_authenticated", format!("the {name} instance's login has expired and cannot be refreshed — {fix}")).with_status(401),
        })
    }
}

impl Resolved {
    /// Why a call cannot be made, before one is: an API-key instance whose
    /// variable is unset. The readiness check is its one caller outside the
    /// wire clients, which keep it as a defensive check of their own.
    pub fn missing_key(&self) -> Option<String> {
        let keyed = self.auth == Auth::ApiKey || (self.auth == Auth::Vendor && !self.api_key_env.is_empty());
        if !keyed || !self.api_key.is_empty() {
            return None;
        }
        let store = connect::connect_command(&self.name, self.kind);
        Some(match &self.stored {
            // A stored reference owns the instance: its variable, not the
            // definition's, is the one to set.
            Stored::Env(v) => format!("no API key for the {} instance — its stored key is ${v}, which is not set; set {v}, or store another with `{store}` (krowk does not fall back to ${} for it)", self.name, self.api_key_env),
            Stored::Command { .. } | Stored::Unreadable(_) => format!("the {} instance's stored key has not been read", self.name),
            Stored::Literal => format!("the {} instance's stored key is empty — store another with `{store}`", self.name),
            // A backend's key is the environment's alone.
            _ if self.backend.is_some() => format!("no API key for the {} instance — set {}", self.name, self.api_key_env),
            _ => format!("no API key for the {} instance — set {}, or store one with `{store}`", self.name, self.api_key_env),
        })
    }
}

/// What can be known without spawning anything: a key, an OAuth login, a
/// binary. None when only the vendor can say — a backend on its own login.
pub fn local(inst: &Resolved, credentials: &Path) -> Option<Readiness> {
    if let Some(b) = &inst.backend
        && b.path.is_none()
    {
        return Some(Readiness::NotInstalled);
    }
    // A stored key's command is run by `check`, not here: this answers
    // without spawning anything.
    match &inst.stored {
        Stored::Unreadable(reason) => return Some(Readiness::Unknown { reason: reason.clone() }),
        Stored::Command { command, .. } if inst.api_key.is_empty() => {
            return keys::ran(command).map(|_| Readiness::Ready { source: expected_source(inst, credentials) });
        }
        Stored::Env(v) if inst.api_key.is_empty() => return Some(Readiness::KeyNotSet { var: v.clone() }),
        _ => {}
    }
    if inst.missing_key().is_some() {
        return Some(Readiness::KeyNotSet { var: inst.api_key_env.clone() });
    }
    match &inst.auth {
        Auth::ApiKey => Some(Readiness::Ready { source: expected_source(inst, credentials) }),
        Auth::Keyless => Some(Readiness::Ready { source: "no key".into() }),
        Auth::OAuth { .. } => Some(oauth_login(inst, credentials)),
        // A keyed backend runs on its key, which is krowk's to check, not a
        // login the vendor holds.
        Auth::Vendor if !inst.api_key_env.is_empty() => Some(Readiness::Ready { source: expected_source(inst, credentials) }),
        Auth::Vendor => None,
    }
}

/// An OAuth login, as krowk's own credentials file holds it. Nothing of
/// the tokens leaves this function but whether they can still be used.
fn oauth_login(inst: &Resolved, credentials: &Path) -> Readiness {
    match oauth::Store::new(credentials.to_path_buf()).load(&inst.name) {
        Err(e) => Readiness::Unknown { reason: e.message },
        Ok(None) => Readiness::NotSignedIn,
        Ok(Some(s)) if s.expired_for_good(krowk_store::now_ms()) => Readiness::Expired,
        Ok(Some(_)) => Readiness::Ready { source: format!("OAuth login in {}", credentials.display()) },
    }
}

/// One instance, asked: locally when that answers, else of its vendor
/// where `probe` says (bounded by its deadline, a "signed in" cached for
/// `CACHE_FOR`). Blocks for as long as the vendor takes.
pub fn check(inst: &Resolved, credentials: &Path, probe: &Probe) -> Report {
    let readiness = local(inst, credentials).unwrap_or_else(|| match (&inst.stored, &inst.backend) {
        // The one command readiness runs: the person's own, for its key.
        (Stored::Command { .. }, _) => match keys::materialise(inst) {
            Ok(_) => Readiness::Ready { source: expected_source(inst, credentials) },
            Err(e) => Readiness::Unknown { reason: e.message },
        },
        (_, Some(b)) => vendor_cached(inst, b, probe),
        (_, None) => Readiness::Unknown { reason: "no way to check this instance".into() },
    });
    report(inst, readiness, credentials)
}

/// Every instance, the vendor checks in parallel: as long as the slowest
/// one, never their sum. In the order given.
pub fn check_all(instances: &[&Resolved], credentials: &Path, probe: &Probe) -> Vec<Report> {
    std::thread::scope(|s| {
        let running: Vec<_> = instances.iter().map(|inst| s.spawn(move || check(inst, credentials, probe))).collect();
        running
            .into_iter()
            .zip(instances)
            .map(|(h, inst)| h.join().unwrap_or_else(|_| report(inst, Readiness::Unknown { reason: "the check panicked".into() }, credentials)))
            .collect()
    })
}

/// `check`, off an async runtime's thread: a vendor check blocks on a
/// process for up to `VENDOR_TIMEOUT`, and the host's runtime has one
/// thread that the TUI's drawing and every other session also run on.
pub async fn check_async(inst: &Resolved, credentials: &Path, probe: &Probe) -> Report {
    if let Some(r) = local(inst, credentials) {
        return report(inst, r, credentials);
    }
    let (inst2, creds, probe) = (inst.clone(), credentials.to_path_buf(), probe.clone());
    match tokio::task::spawn_blocking(move || check(&inst2, &creds, &probe)).await {
        Ok(r) => r,
        Err(_) => report(inst, Readiness::Unknown { reason: "the check did not finish".into() }, credentials),
    }
}

/// Where a turn runs when it was asked for by a bare id — or, with nothing
/// asked, on the default (`defaultModel`, routed the same way when it is
/// bare). No instance is ranked above another: any fixed order would
/// silently send `sonnet` to a pay-per-token key when the person also has
/// a subscription, or the other way round. So, of the instances that serve
/// the id (`Registry::candidates`):
///
/// 1. the session's own (`current`), then the one `defaultModel` names —
///    each when it serves the id and is ready: the person already chose it;
/// 2. else the one that is ready, when exactly one is;
/// 3. else, when several are, `ambiguous_model`, listing them with what
///    each is and the `--model` that picks it; when none is, `none_ready`,
///    naming each and what connects it.
///
/// A vendor that could not tell (`unknown`) is a candidate like a ready
/// one — alone it is taken, as the host's own check leaves such a turn to
/// run; beside another it makes the choice ambiguous. The id is then
/// spelled as the chosen instance takes it (`Registry::model_on`); with no
/// id, it is the instance's default model. An explicit `<instance>/<model>`
/// comes back as it is, never rerouted and never checked here — the turn's
/// own readiness check names what is missing.
///
/// Every candidate is asked once: a key, a keyless server or an OAuth login
/// without a process, and the backends together in one parallel pass
/// (`check_all`), each "signed in" served from the cache for `CACHE_FOR` —
/// none at all when the first preferred instance (the session's, else the
/// default's) is ready by its key. `probe` is where the vendors are asked, by this module's rules:
/// the session's own directory only once its repository is trusted, else
/// krowk's own (`neutral_dir`) — routing never asks a trust question.
pub fn route(
    reg: &Registry,
    asked: Option<&Asked>,
    current: Option<&ModelRef>,
    credentials: &Path,
    probe: &Probe,
    listed: &dyn Fn(&str) -> Vec<Listed>,
) -> Result<ModelRef, EngineError> {
    let configured;
    let asked = match asked {
        Some(a) => Some(a),
        None => match &reg.default_model {
            Some(d) => {
                configured = reg.read_model(d).map_err(|e| EngineError::new("bad_config", format!("config defaultModel: {e}")))?;
                Some(&configured)
            }
            None => None,
        },
    };
    let bare = match asked {
        Some(Asked::Exact(m)) => return Ok(m.clone()),
        Some(Asked::Bare(b)) => Some(b.as_str()),
        None => None,
    };
    let cands = reg.candidates(bare);
    let default = match reg.default_model.as_deref().map(|d| reg.read_model(d)) {
        Some(Ok(Asked::Exact(d))) => Some(d.instance),
        _ => None,
    };
    let preferred: Vec<&str> = current.map(|c| c.instance.as_str()).into_iter().chain(default.as_deref()).filter(|p| cands.iter().any(|c| c.name == *p)).collect();
    let spell = |inst: &Resolved| {
        reg.model_on(inst, bare, listed).map(|model| ModelRef { instance: inst.name.clone(), model }).map_err(|why| EngineError::new("bad_model", why))
    };
    // The first preferred instance ready by its key needs no vendor asked:
    // nothing ranked below it could win.
    if let Some(inst) = preferred.first().and_then(|p| reg.instances.get(*p))
        && local(inst, credentials).is_some_and(|r| r.is_ready())
    {
        return spell(inst);
    }
    let locals: Vec<Option<Readiness>> = cands.iter().map(|i| local(i, credentials)).collect();
    let ask: Vec<&Resolved> = cands.iter().zip(&locals).filter(|(_, l)| l.is_none()).map(|(i, _)| *i).collect();
    let mut answers = check_all(&ask, credentials, probe).into_iter();
    let rows: Vec<(&Resolved, Report)> = cands
        .iter()
        .zip(locals)
        .map(|(i, l)| {
            let r = match l {
                Some(r) => report(i, r, credentials),
                None => answers.next().unwrap_or_else(|| report(i, Readiness::Unknown { reason: "not asked".into() }, credentials)),
            };
            (*i, r)
        })
        .collect();
    let ready: Vec<&Resolved> = rows.iter().filter(|(_, r)| r.readiness.is_ready()).map(|(i, _)| *i).collect();
    if let Some(inst) = preferred.iter().find_map(|p| ready.iter().find(|i| i.name == *p)) {
        return spell(inst);
    }
    // One that could not be checked is a candidate beside the ready ones:
    // a subscription whose check timed out may well be signed in, and a
    // ready key winning over it would be the guess this rule refuses.
    let unknown: Vec<&Resolved> = rows.iter().filter(|(_, r)| matches!(r.readiness, Readiness::Unknown { .. })).map(|(i, _)| *i).collect();
    let pool: Vec<&Resolved> = ready.iter().chain(&unknown).copied().collect();
    let what = crate::instances::serving(bare).0;
    let asked = match bare {
        Some(b) => format!("{b:?}"),
        None => "a model (no --model, and config names no defaultModel)".into(),
    };
    match pool.as_slice() {
        [one] => spell(one),
        [] => {
            let run: Vec<String> = crate::instances::serving(bare)
                .1
                .iter()
                .filter_map(|v| reg.instances.get(*v))
                .filter_map(|i| suggested(i).map(|c| format!("`{c}` ({})", kind_label(i.kind))))
                .collect();
            let run = match run.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => String::new(),
            };
            // A router or a compatible server that is ready may serve it:
            // krowk cannot tell, but the person can name it.
            let named: Vec<String> = reg
                .instances
                .values()
                .filter(|i| i.backend.is_none() && suggested(i).is_none() && local(i, credentials).is_some_and(|r| r.is_ready()))
                .map(|i| format!("`--model {}/<id>`", i.name))
                .collect();
            let named = if named.is_empty() { String::new() } else { format!(", or name one that may serve it: {}", named.join(", ")) };
            let needs: Vec<String> = rows.iter().map(|(i, r)| format!("  {}, {} ({}): {}", i.name, kind_label(i.kind), r.readiness.label(), r.fix.clone().unwrap_or_default())).collect();
            let for_what = if bare.is_some() { format!(" serves {what} (asked for {asked})") } else { " can run a model here (no --model, and config names no defaultModel)".into() };
            Err(EngineError::new("none_ready", format!("no connected instance{for_what} — run {run}{named}. What each one needs:\n{}", needs.join("\n"))))
        }
        several => {
            let picks: Vec<String> = several
                .iter()
                .map(|i| {
                    let m = reg.model_on(i, bare, listed).unwrap_or_else(|_| "<model>".into());
                    let unchecked = if unknown.iter().any(|u| u.name == i.name) { " (could not be checked — see `krowk status`)" } else { "" };
                    format!("  {}, {}: --model {}/{m}{unchecked}", i.name, kind_label(i.kind), i.name)
                })
                .collect();
            let example = several.iter().find_map(|i| suggested(i)).map(|c| format!(" (e.g. `{c} --default`)")).unwrap_or_default();
            Err(EngineError::new(
                "ambiguous_model",
                format!(
                    "{} connected instances could run {asked}, and krowk does not choose between them for you — pick one with --model <instance>/<id>:\n{}\nor make one the default with `krowk connect <vendor> --default`{example}",
                    several.len(),
                    picks.join("\n")
                ),
            ))
        }
    }
}

/// A readiness with its source and fix around it: the row every caller
/// prints or refuses with.
pub fn report(inst: &Resolved, readiness: Readiness, credentials: &Path) -> Report {
    let source = match &readiness {
        Readiness::Ready { source } => source.clone(),
        _ => expected_source(inst, credentials),
    };
    let fix = fix(inst, &readiness);
    Report { instance: inst.name.clone(), kind: inst.kind, readiness, source, fix }
}

/// Where the credential would come from, said of an instance that is not
/// ready, so the person knows which variable or login to look at.
fn expected_source(inst: &Resolved, credentials: &Path) -> String {
    match &inst.auth {
        Auth::ApiKey => inst.stored.source().unwrap_or_else(|| format!("env {}", inst.api_key_env)),
        Auth::Keyless => "no key".into(),
        Auth::OAuth { .. } => format!("OAuth login in {}", credentials.display()),
        Auth::Vendor if !inst.api_key_env.is_empty() => format!("env {}, handed to {}", inst.api_key_env, inst.vendor),
        Auth::Vendor => vendor_login_source(inst, None),
    }
}

/// "Claude Code's own login in ~/.claude", with what it said when it is one.
fn vendor_login_source(inst: &Resolved, said: Option<&str>) -> String {
    let at = inst.backend.as_ref().and_then(|b| b.home.as_ref()).map(|h| format!(" in {}", h.display())).unwrap_or_default();
    match said {
        Some(s) => format!("{}'s own login{at} ({s})", inst.vendor),
        None => format!("{}'s own login{at}", inst.vendor),
    }
}

fn fix(inst: &Resolved, r: &Readiness) -> Option<String> {
    let codex = inst.wire_api == WireApi::CodexAppServer;
    Some(match r {
        Readiness::Ready { .. } => return None,
        Readiness::KeyNotSet { var } if inst.backend.is_some() => format!("set {var}"),
        Readiness::KeyNotSet { var } => format!("set {var}, or store a key with `{}`", connect::connect_command(&inst.name, inst.kind)),
        Readiness::NotSignedIn | Readiness::Expired if matches!(inst.auth, Auth::OAuth { .. }) => format!("sign in with `{}`", connect::connect_command(&inst.name, inst.kind)),
        Readiness::NotSignedIn | Readiness::Expired => {
            let own = if codex { "Codex's" } else { "Claude's" };
            format!("sign in with `{}`, which runs {own} own login", connect::connect_command(&inst.name, inst.kind))
        }
        Readiness::NotInstalled if codex => format!("install Codex (https://developers.openai.com/codex), or name the binary with `{} --binary <path>`", connect::connect_command(&inst.name, inst.kind)),
        Readiness::NotInstalled => format!("install Claude Code (https://claude.com/claude-code), or name the binary with `{} --binary <path>`", connect::connect_command(&inst.name, inst.kind)),
        Readiness::Unknown { .. } => "see the reason, then run `krowk status` again".into(),
    })
}

/// The command routing suggests for an instance — `connect::connect_command`,
/// the one every fix line uses — for a kind `krowk connect` sets up by a
/// vendor's own method. None for a router or a compatible server, whose
/// models krowk cannot tell: routing only ever names those.
fn suggested(i: &Resolved) -> Option<String> {
    (!matches!(i.kind, "openrouter-api" | "openai-compatible")).then(|| connect::connect_command(&i.name, i.kind))
}

/// Vendor answers that said "signed in", by instance, and when.
static SIGNED_IN: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);

/// What makes two vendor checks the same question: the instance, the
/// directory it is asked in (a project's settings can change the answer),
/// and everything that changes which login the vendor reads. No key value
/// is in it — a keyed backend never reaches the vendor check.
fn cache_key(inst: &Resolved, b: &Backend, probe: &Probe) -> String {
    format!("{}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}", inst.name, probe.dir, b.path, b.config_dir, b.env, b.args)
}

fn vendor_cached(inst: &Resolved, b: &Backend, probe: &Probe) -> Readiness {
    let key = cache_key(inst, b, probe);
    let known = SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)).filter(|(at, _)| at.elapsed() < CACHE_FOR).map(|(_, s)| s.clone());
    if let Some(source) = known {
        return Readiness::Ready { source };
    }
    let r = vendor(inst, b, probe);
    if let Readiness::Ready { source } = &r {
        SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, (Instant::now(), source.clone()));
    }
    r
}

/// Forgets what was believed of an instance's vendor login, in every
/// directory it was asked in: a sign-in or a sign-out has just changed it,
/// and a long-lived host (the TUI) asks again rather than believe the old
/// answer for another minute.
pub fn forget(instance: &str) {
    if let Some(m) = SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.retain(|k, _| k.split('\0').next() != Some(instance));
    }
}

/// Asks the vendor. Only whether there is a login and its kind come back;
/// an email, a plan's owner, the part of a key Codex prints, stay out.
fn vendor(inst: &Resolved, b: &Backend, probe: &Probe) -> Readiness {
    let said = match inst.wire_api {
        WireApi::CodexAppServer => codex_auth::signed_in(b, probe).map(|st| (st.logged_in, st.describe())),
        _ => claude_auth::status(b, probe).map(|st| (st.logged_in, st.describe())),
    };
    match said {
        Ok((true, how)) => Readiness::Ready { source: vendor_login_source(inst, Some(&how)) },
        Ok((false, _)) => Readiness::NotSignedIn,
        Err(reason) => Readiness::Unknown { reason },
    }
}

/// A vendor process for a check: started in `probe.dir` — never wherever
/// krowk happens to run, whose project settings nobody may have trusted —
/// in a process group of its own, so the whole of it can be stopped. The
/// group is unix's: on Windows only the process krowk started is stopped,
/// and one it started in turn (`claude.cmd`'s Node) can outlive the check
/// until it exits by itself — a Job object would close that, and is not
/// worth a new dependency for a status check.
///
/// The group is registered (`group::register`) until the check lets it
/// go, so a krowk that leaves while a check is still running — the TUI
/// quit while it routes — kills it on the way out (`group::kill_all`)
/// rather than leaving it to its deadline, whose thread is gone with the
/// process.
pub(crate) fn probing(cmd: &mut Command, probe: &Probe) -> std::io::Result<std::process::Child> {
    probing_as(cmd, probe, false)
}

/// `probing`, and with `detached` in a session of its own: no controlling
/// terminal at all, so a command that would ask on `/dev/tty` fails at once
/// rather than drawing a prompt it can never read. Its session's id is its
/// process group's, so it is stopped the same way.
fn probing_as(cmd: &mut Command, probe: &Probe, detached: bool) -> std::io::Result<std::process::Child> {
    cmd.current_dir(&probe.dir);
    #[cfg(unix)]
    if detached {
        // SAFETY: setsid is async-signal-safe and touches no memory of the
        // parent's; it runs in the child between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(cmd, || if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
        }
    } else {
        std::os::unix::process::CommandExt::process_group(cmd, 0);
    }
    #[cfg(not(unix))]
    let _ = detached;
    let child = cmd.spawn()?;
    crate::group::register(Some(child.id()));
    Ok(child)
}

/// Stops a check's process and everything it started — a Node runtime's
/// workers, a shell's `sleep` — and reaps it: nothing outlives a check
/// (on unix; see `probing`).
pub(crate) fn stop(child: &mut std::process::Child) {
    kill_group(child);
    let _ = child.kill();
    // Released before it is reaped: once reaped, its pid is anyone's.
    crate::group::release(Some(child.id()));
    let _ = child.wait();
}

/// SIGKILL to the process group `child` leads (`probing` made it its own).
/// Also after the leader exited: whatever it left behind in its group
/// would otherwise hold the output pipes, and the check with them.
pub(crate) fn kill_group(child: &std::process::Child) {
    #[cfg(unix)]
    // SAFETY: kill with a negative pid signals a process group; it touches
    // no memory. A group with nobody left in it is ESRCH, ignored.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Runs a check's command to its end, or stops it at `probe.within`: none
/// then. Its output is read while it runs, so a chatty vendor cannot fill a
/// pipe and hang on it, and read only until the deadline even once it has
/// exited: a process it left behind that escaped its group (`setsid`, a
/// daemonizing credential helper) can hold the pipes open for good, and is
/// then abandoned with whatever had been read — the check never waits on
/// it past the deadline. Each stream is kept to `OUTPUT_CAP`; stdout past
/// it stops the command and is an error.
pub(crate) fn output_within(cmd: &mut Command, probe: &Probe) -> std::io::Result<Option<Output>> {
    output_as(cmd, probe, false)
}

/// `output_within`, with no controlling terminal (`probing_as`): a stored
/// key's command.
pub(crate) fn output_detached(cmd: &mut Command, probe: &Probe) -> std::io::Result<Option<Output>> {
    output_as(cmd, probe, true)
}

fn output_as(cmd: &mut Command, probe: &Probe, detached: bool) -> std::io::Result<Option<Output>> {
    use std::io::Read;
    use std::sync::{mpsc, Arc};
    let within = probe.within;
    let mut child = probing_as(cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()), probe, detached)?;
    // Each pipe is read into a buffer shared with this thread, and says
    // when it reached its end.
    // Each stream is kept to OUTPUT_CAP: past it stdout is an overflow the
    // check fails on, and stderr is read on and dropped — a vendor's answer
    // and a key are both far smaller, and nothing may grow without bound.
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain = |pipe: Option<Box<dyn Read + Send>>, over: Option<Arc<std::sync::atomic::AtomicBool>>| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel::<()>();
        let into = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = p.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = into.lock().unwrap_or_else(|e| e.into_inner());
                    if b.len() + n > OUTPUT_CAP {
                        if let Some(o) = &over {
                            o.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        continue;
                    }
                    b.extend_from_slice(&chunk[..n]);
                }
            }
            let _ = done.send(());
        });
        (buf, finished)
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), Some(overflow.clone()));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>), None);
    let started = Instant::now();
    loop {
        let gone = match exited(&mut child) {
            Ok(gone) => gone,
            // Stopped and reaped before the error goes up, so nothing is
            // left running or unreaped (ECHILD when SIGCHLD is ignored).
            Err(e) => {
                stop(&mut child);
                return Err(e);
            }
        };
        if gone {
            // The vendor answered and left, not yet reaped, so its pid —
            // the group's id — is still its own: whatever it left running
            // in the group, holding the pipes the answer is read from, is
            // stopped before the pid can go to anyone else.
            kill_group(&child);
            crate::group::release(Some(child.id()));
            break;
        }
        if overflow.load(std::sync::atomic::Ordering::Relaxed) {
            stop(&mut child);
            return Err(std::io::Error::other(format!("it printed more than {} KiB", OUTPUT_CAP / 1024)));
        }
        if started.elapsed() > within {
            stop(&mut child);
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.wait()?;
    let take = |(buf, finished): (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>)| {
        // A small floor: a vendor that answered right at the deadline
        // still has its last bytes copied; a leftover holding the pipe is
        // still abandoned. The reader it leaves blocks until that leftover
        // exits — a thread per stuck check, bounded by the signed-in memo.
        let _ = finished.recv_timeout(within.saturating_sub(started.elapsed()).max(Duration::from_millis(50)));
        std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let (stdout, stderr) = (take(out), take(err));
    if overflow.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(std::io::Error::other(format!("it printed more than {} KiB", OUTPUT_CAP / 1024)));
    }
    Ok(Some(Output { status, stdout, stderr }))
}

/// Whether the child has exited, without reaping it: its pid stays its own
/// (a zombie) until `wait`, so `kill_group` cannot reach a group id the
/// system has since handed to someone else.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: waitid writes only into the zeroed siginfo it is handed;
    // WNOWAIT leaves the child to be reaped by `wait`.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

/// Elsewhere `try_wait` reaps the child as it reports it, so the group is
/// signalled after its leader's pid is free. Accepted: the pid would have
/// to be reused, as a group id, within the few microseconds in between.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    Ok(child.try_wait()?.is_some())
}
