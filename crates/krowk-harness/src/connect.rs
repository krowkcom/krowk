//! Connecting a model source, and disconnecting it — one way for every front
//! end: `krowk connect`, `krowk disconnect` and `krowk providers add|remove`
//! on the command line, and the TUI's `/connect` and `/disconnect`. Before
//! this the sign-in lived in the CLI's `providers add`, where nothing else
//! could reach it.
//!
//! A person connects a **vendor** by a **method**, not a backend kind:
//! Anthropic by a Claude subscription (a Claude Code account, `claude:work`,
//! signed in by `claude auth login`) or an API key (`anthropic:work`);
//! OpenAI by a ChatGPT subscription (a Codex account, signed in by `codex
//! login`, or `codex login --device-auth`) or an API key; xAI by SuperGrok
//! (krowk's own OAuth, a browser or a device code) or an API key; OpenRouter
//! and an OpenAI-compatible server by an API key. `METHODS` is that table,
//! and each row names the provider its instances are called after — the
//! word `krowk providers add` takes.
//!
//! **krowk never signs in to a subscription itself.** A Claude or ChatGPT
//! login is made by the vendor's own CLI on the person's own terminal and
//! asked about by that CLI; krowk never reads its files (R-BACK-2,
//! R-BACK-3). Those methods say they need the real terminal
//! (`MethodInfo::terminal`), and run inside `AuthInteraction::terminal`, so a
//! front end that owns the screen gives it up for them and takes it back.
//!
//! What a method needs to ask or tell goes through `AuthInteraction`: a
//! prompt (text, a secret, a choice) and a notice (a line of information, a
//! URL to open, a device code, progress). The CLI answers at the terminal,
//! or — with nobody there — fails with the flag that would have answered;
//! the TUI answers in an overlay. No method prints anything itself.
//!
//! An API key is read from the environment variable its definition names;
//! connecting one writes the definition and says which variable. The
//! `Secret` prompt is where a pasted key will come in when krowk stores
//! keys; nothing asks it yet.
//!
//! **A failed sign-in writes nothing**: no definition, and no directory
//! left behind of the ones it made. Connecting an instance that exists
//! renews its login and keeps its definition, with the fields given
//! replacing the ones they name.

use crate::claude::auth as claude_auth;
use crate::codex::{self, auth as codex_auth};
use crate::engine::EngineError;
use crate::instances::{self, Auth, InstanceKind, InstancesConfig, Registry, Resolved};
use crate::oauth::{self, Step, Store};
use crate::readiness::{self, Probe, Report};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// Who sells the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Anthropic,
    Openai,
    Xai,
    Openrouter,
    OpenaiCompatible,
}

impl Vendor {
    pub const ALL: [Vendor; 5] = [Vendor::Anthropic, Vendor::Openai, Vendor::Xai, Vendor::Openrouter, Vendor::OpenaiCompatible];

    /// As `krowk connect` takes it.
    pub fn id(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::Openai => "openai",
            Vendor::Xai => "xai",
            Vendor::Openrouter => "openrouter",
            Vendor::OpenaiCompatible => "openai-compatible",
        }
    }

    /// As a picker shows it.
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Anthropic => "Anthropic — Claude",
            Vendor::Openai => "OpenAI — ChatGPT, GPT",
            Vendor::Xai => "xAI — Grok",
            Vendor::Openrouter => "OpenRouter",
            Vendor::OpenaiCompatible => "A server that speaks OpenAI's API — a local model, a gateway",
        }
    }

    pub fn parse(s: &str) -> Option<Vendor> {
        let s = s.trim().to_ascii_lowercase();
        Vendor::ALL.into_iter().find(|v| v.id() == s)
    }

    /// The ways in, in the order a picker offers them.
    pub fn methods(self) -> Vec<&'static MethodInfo> {
        METHODS.iter().filter(|m| m.vendor == self).collect()
    }
}

/// How a vendor is signed in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The vendor's own subscription login: Claude, ChatGPT, SuperGrok.
    Subscription,
    /// The same, with a code typed into a browser anywhere.
    Device,
    /// A key, read from an environment variable.
    ApiKey,
}

impl Method {
    pub fn id(self) -> &'static str {
        match self {
            Method::Subscription => "subscription",
            Method::Device => "device",
            Method::ApiKey => "api-key",
        }
    }

    pub fn parse(s: &str) -> Option<Method> {
        match s.trim().to_ascii_lowercase().as_str() {
            "subscription" => Some(Method::Subscription),
            "device" => Some(Method::Device),
            "api-key" => Some(Method::ApiKey),
            _ => None,
        }
    }
}

/// One way into one vendor.
#[derive(Debug, PartialEq, Eq)]
pub struct MethodInfo {
    pub vendor: Vendor,
    pub method: Method,
    /// The provider its instances are named after — `claude`, `claude:work`
    /// — and the word `krowk providers add` takes for it.
    pub provider: &'static str,
    /// The `kind` of the definitions it makes.
    pub kind: &'static str,
    pub label: &'static str,
    /// Hands the terminal to the vendor's own CLI (`claude auth login`,
    /// `codex login`), inside `AuthInteraction::terminal`.
    pub terminal: bool,
}

const fn way(vendor: Vendor, method: Method, provider: &'static str, kind: &'static str, label: &'static str, terminal: bool) -> MethodInfo {
    MethodInfo { vendor, method, provider, kind, label, terminal }
}

/// Every vendor's ways in.
pub const METHODS: &[MethodInfo] = &[
    way(Vendor::Anthropic, Method::Subscription, "claude", "claude-code", "Claude subscription (Pro, Max, Team) — Claude Code's own login", true),
    way(Vendor::Anthropic, Method::ApiKey, "anthropic", "anthropic-api", "Anthropic API key", false),
    way(Vendor::Openai, Method::Subscription, "codex", "codex-app-server", "ChatGPT subscription — Codex's own login", true),
    way(Vendor::Openai, Method::Device, "codex", "codex-app-server", "ChatGPT subscription, with a code typed into any browser — Codex's own login", true),
    way(Vendor::Openai, Method::ApiKey, "openai", "openai-api", "OpenAI API key", false),
    way(Vendor::Xai, Method::Subscription, "supergrok", "xai-oauth", "SuperGrok or X Premium subscription", false),
    way(Vendor::Xai, Method::Device, "supergrok", "xai-oauth", "SuperGrok or X Premium, with a code typed into any browser", false),
    way(Vendor::Xai, Method::ApiKey, "xai", "xai-api", "xAI API key", false),
    way(Vendor::Openrouter, Method::ApiKey, "openrouter", "openrouter-api", "OpenRouter API key", false),
    way(Vendor::OpenaiCompatible, Method::ApiKey, "openai-compatible", "openai-compatible", "Its base URL, and the variable its key is in if it takes one", false),
];

/// `krowk providers add <provider>`'s provider as a way in: `--device`
/// picks the device variant where there is one.
pub fn by_provider(provider: &str, device: bool) -> Option<&'static MethodInfo> {
    let of = |m: &&MethodInfo| m.provider == provider;
    METHODS.iter().filter(of).find(|m| (m.method == Method::Device) == device).or_else(|| METHODS.iter().find(of))
}

/// The way an existing definition was connected.
fn by_kind(kind: &str) -> Option<&'static MethodInfo> {
    METHODS.iter().find(|m| m.kind == kind)
}

/// The command that connects an instance, or renews its login, for a fix
/// line: `krowk connect <vendor> [--method M] [--name N]` — the method
/// only when the vendor has more than one (`instances::kind_connect`) — or
/// `krowk connect <instance>` for one whose name no `--name` spells (a
/// hand-written `grok:team`).
pub fn connect_command(instance: &str, kind: &str) -> String {
    let (Some(m), Some((vendor, method))) = (by_kind(kind), instances::kind_connect(kind)) else { return format!("krowk connect {instance}") };
    let method = if m.vendor.methods().len() > 1 { format!(" --method {method}") } else { String::new() };
    let name = match instance.strip_prefix(m.provider).and_then(|r| r.strip_prefix(':')) {
        _ if instance == m.provider => String::new(),
        Some(n) if !n.contains(':') => format!(" --name {n}"),
        _ if m.provider == "openai-compatible" && !instance.contains(':') => format!(" --name {instance}"),
        _ => return format!("krowk connect {instance}"),
    };
    format!("krowk connect {vendor}{method}{name}")
}

/// A new instance's whole name from `--name`: `<provider>:<name>`, or the
/// name alone for a compatible server, which has no provider of its own. A
/// name with a `:` must be that same whole name — `claude:work` for a
/// subscription — so it can never pass for another method's instance, and
/// the part after the prefix holds no `:` of its own: an account's
/// directory is made from it, and `a:b` and `a-b` would make the same one.
fn new_instance(provider: &str, name: &str) -> Result<String, EngineError> {
    check_name(name)?;
    let rest = match name.split_once(':') {
        None => name,
        Some((pre, rest)) if pre == provider && provider != "openai-compatible" => rest,
        Some(_) if provider == "openai-compatible" => return Err(bad_flag(format!("{name:?} cannot name a server: its name is the whole instance name, so no `:`"))),
        Some(_) => return Err(bad_flag(format!("{name:?} is not a {provider} name — give --name the part after `{provider}:`, e.g. --name work"))),
    };
    if rest.is_empty() || rest.contains(':') {
        return Err(bad_flag(format!("{name:?} cannot name an account: the part after `{provider}:` holds no `:`")));
    }
    Ok(if provider == "openai-compatible" { rest.to_string() } else { format!("{provider}:{rest}") })
}

/// The model a new instance of this kind is set up to run when it becomes
/// the default: none for a server whose models are its own.
pub fn default_model(kind: &str) -> Option<&'static str> {
    // The router's own defaults, and OpenRouter's, which the router never
    // picks by itself (it cannot tell what a router serves) but a
    // connection to it can name.
    instances::default_model_of(kind).or((kind == "openrouter-api").then_some("openai/gpt-5.4"))
}

/// Whether a connection makes its instance the default model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakeDefault {
    /// Only when config.json names no `defaultModel` yet: the first
    /// connection is what a bare `krowk` runs on.
    IfUnset,
    /// Whatever it names (`krowk connect --default`).
    Always,
    /// Never (`krowk providers add`, which only writes a definition).
    Never,
}

/// What a way in asks.
pub enum Prompt<'a> {
    /// A line of text. `flag` is what answers it without a prompt.
    Text { message: &'a str, flag: &'a str },
    /// A secret: never echoed, never logged.
    Secret { message: &'a str, flag: &'a str },
    /// One of `options`, by index.
    Select { message: &'a str, options: &'a [&'a str], flag: &'a str },
}

pub enum Answer {
    Text(String),
    Choice(usize),
}

/// What a way in tells the person while it runs.
pub enum Notice<'a> {
    Info(&'a str),
    /// A page to sign in on: opened for the person where that can be done.
    AuthUrl { url: &'a str, message: &'a str },
    /// A code to enter at `url`, on any device.
    DeviceCode { url: &'a str, code: &'a str, message: &'a str },
    Progress(&'a str),
}

/// The person, as a front end reaches them.
pub trait AuthInteraction {
    /// Whether there is anybody to ask. Without, a question that has a
    /// default (which account) takes it, and one that has none fails.
    fn interactive(&self) -> bool {
        true
    }
    /// Asks. With nobody to ask, fails with what `flag` says answers it.
    fn prompt(&mut self, prompt: Prompt<'_>) -> Result<Answer, EngineError>;
    fn notify(&mut self, notice: Notice<'_>);
    /// Runs a vendor's own login, which reads and draws on the real
    /// terminal: a front end that owns the screen gives it up for `run`
    /// and takes it back after. The CLI's terminal is already the real one.
    fn terminal(&mut self, run: &mut dyn FnMut() -> Result<ExitStatus, String>) -> Result<ExitStatus, String> {
        run()
    }
}

fn wrong_answer() -> EngineError {
    EngineError::new("bad_argument", "the answer did not fit the question")
}

fn ask_text(ui: &mut dyn AuthInteraction, message: &str, flag: &str) -> Result<String, EngineError> {
    match ui.prompt(Prompt::Text { message, flag })? {
        Answer::Text(t) => Ok(t.trim().to_string()),
        Answer::Choice(_) => Err(wrong_answer()),
    }
}

fn choose(ui: &mut dyn AuthInteraction, message: &str, options: &[&str], flag: &str) -> Result<usize, EngineError> {
    match ui.prompt(Prompt::Select { message, options, flag })? {
        Answer::Choice(i) if i < options.len() => Ok(i),
        _ => Err(wrong_answer()),
    }
}

/// What `--name`, `--api-key-env`, `--base-url` and the rest said.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub name: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub client_id: Option<String>,
    pub binary: Option<String>,
    pub config_dir: Option<String>,
}

/// One connection to make: a way in, and the instance it makes or renews —
/// named whole by `instance`, else by `options.name` after the provider.
pub struct Request {
    pub method: &'static MethodInfo,
    pub instance: Option<String>,
    pub options: Options,
    pub default: MakeDefault,
}

/// What a vendor CLI said of an account, less anything personal.
#[derive(Debug, Clone)]
pub struct VendorLogin {
    pub logged_in: bool,
    pub describe: String,
    /// Whether its own login ran: no when it was signed in already, or runs
    /// on a key.
    pub ran: bool,
    /// A Codex account's links to the person's own Codex configuration.
    pub shared: Vec<String>,
}

/// A connection made.
pub struct Connected {
    pub instance: String,
    pub definition: InstanceKind,
    pub resolved: Resolved,
    /// It existed, and this renewed it.
    pub renewed: bool,
    /// A SuperGrok login, its tokens in the credentials file.
    pub oauth: bool,
    pub vendor: Option<VendorLogin>,
    /// `defaultModel`, when this connection set it.
    pub default_model: Option<String>,
}

/// What `disconnect` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignedOut {
    /// krowk's own OAuth tokens, deleted; `had` whether there were any.
    Tokens { had: bool },
    /// The vendor's own sign-out ran for the account's directory.
    Vendor { command: &'static str, home: Option<PathBuf> },
    /// A key krowk only reads from `var`: unsetting it is the person's.
    Key { var: String },
    /// Nothing to sign out of.
    Keyless,
}

pub struct Disconnected {
    pub instance: String,
    pub kind: &'static str,
    pub signed_out: SignedOut,
    pub removed_definition: bool,
    /// The `defaultModel` taken away with the definition it ran on.
    pub cleared_default: Option<String>,
}

/// What `remove` (`krowk providers remove`) took away.
pub struct Removed {
    pub definition: bool,
    pub login: bool,
    /// A Claude Code or Codex account's directory, kept with the vendor's
    /// login in it, and whether it is Codex's.
    pub kept: Option<(String, bool)>,
}

/// Connecting and disconnecting, against krowk's config (`config.json`)
/// and provider credentials file, in the environment krowk resolved.
pub struct ProviderAuth<'a> {
    pub config: PathBuf,
    pub credentials: PathBuf,
    pub env: &'a dyn Fn(&str) -> String,
}

fn bad_flag(m: impl Into<String>) -> EngineError {
    EngineError::new("bad_flag", m)
}

fn backend_failed(m: impl Into<String>) -> EngineError {
    EngineError::new("backend_failed", m)
}

fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// `NAME` in `OPENAI_NAME_API_KEY`: upper case, anything else an underscore.
fn env_part(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect()
}

/// An instance's name is the part of `--model` before the model id, and a
/// named account's directory is made from it, so it cannot hold a `/`, a
/// `\`, a space or a control character, nor be `.` or `..`.
fn check_name(name: &str) -> Result<(), EngineError> {
    if name.is_empty() || name.chars().any(|c| c == '/' || c == '\\' || c.is_whitespace() || c.is_control()) || matches!(name, "." | "..") {
        return Err(bad_flag(format!("{name:?} cannot name an instance: it is the part of --model before the model id, so no `/`, `\\` or space")));
    }
    Ok(())
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

impl ProviderAuth<'_> {
    /// The ways into a vendor.
    pub fn methods(vendor: Vendor) -> Vec<&'static MethodInfo> {
        vendor.methods()
    }

    fn env(&self, k: &str) -> String {
        (self.env)(k)
    }

    fn raw_config(&self) -> Result<Map<String, Value>, EngineError> {
        let bad = |m: String| EngineError::new("bad_config", m);
        match std::fs::read(&self.config) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| bad(format!("{} is not valid JSON: {e}", self.config.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(e) => Err(bad(format!("reading {}: {e}", self.config.display()))),
        }
    }

    /// The instances config.json defines.
    pub fn definitions(&self) -> Result<InstancesConfig, EngineError> {
        instances::from_config_json(&Value::Object(self.raw_config()?)).map_err(|e| EngineError::new("bad_config", format!("{}: {e}", self.config.display())))
    }

    /// Writes one definition, or takes it away, keeping every other key of
    /// config.json as it was; replaced by rename, never in place.
    fn write(&self, instance: &str, def: Option<&InstanceKind>) -> Result<(), EngineError> {
        self.edit(|raw| {
            let all = raw.entry("instances").or_insert_with(|| Value::Object(Map::new()));
            if !all.is_object() {
                *all = Value::Object(Map::new());
            }
            let all = all.as_object_mut().expect("an object");
            match def {
                Some(d) => {
                    all.insert(instance.into(), serde_json::to_value(d).expect("a definition serializes"));
                }
                None => {
                    all.remove(instance);
                }
            }
        })
    }

    /// Read, edit, write back by rename: every key the edit does not touch
    /// is kept as it was. The same as `krowk::config::edit` in the CLI crate,
    /// which this crate cannot reach (the CLI depends on it, and its config
    /// module is in the lean build, which links no harness); a change to one
    /// is a change to the other.
    fn edit(&self, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<(), EngineError> {
        let unwritable = |e: String| EngineError::new("config_unwritable", format!("{}: {e}", self.config.display()));
        let mut raw = self.raw_config()?;
        edit(&mut raw);
        let data = serde_json::to_string_pretty(&raw).expect("config serializes") + "\n";
        let dir = self.config.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(".config-{}-connect.json", std::process::id()));
        let result = (|| {
            std::fs::create_dir_all(dir)?;
            std::fs::write(&tmp, data.as_bytes())?;
            #[cfg(unix)]
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &self.config)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|e| unwritable(e.to_string()))
    }

    fn data_dir(&self) -> Result<PathBuf, EngineError> {
        crate::log::sessions_dir(self.env)
            .and_then(|d| d.parent().map(Path::to_path_buf))
            .ok_or_else(|| EngineError::new("no_home", "krowk has no data directory to keep an account in — set HOME or XDG_DATA_HOME"))
    }

    /// Where a vendor is asked: krowk's own directory, as `krowk status`
    /// asks, never the one krowk runs in.
    fn probe(&self) -> Result<Probe, EngineError> {
        readiness::neutral_dir(&self.data_dir()?).map(Probe::at).map_err(|e| EngineError::new("data_dir_unwritable", e))
    }

    fn resolve(&self, instance: &str, kind: &InstanceKind) -> Result<Resolved, EngineError> {
        let reg = Registry::resolve(&InstancesConfig { instances: [(instance.to_string(), kind.clone())].into(), ..Default::default() }, self.env);
        reg.get(instance).cloned().map_err(|e| EngineError::new("bad_config", e))
    }

    /// The person's own vendor login this instance runs on, when it does:
    /// a Claude Code or Codex instance with no directory of its own (or one
    /// naming the vendor's default) signs in and out in the directory
    /// every other tool on the machine uses — `~/.claude`, `~/.codex`.
    /// None for a named account, or a keyed one, which runs no login. With
    /// no directory of its own, always some: the one the environment names,
    /// else `~/.claude` in words when there is no HOME to find it in.
    pub fn own_home(&self, r: &Resolved) -> Option<PathBuf> {
        let b = r.backend.as_ref().filter(|b| b.key.is_none())?;
        let (var, dot) = if r.kind == "codex-app-server" { ("CODEX_HOME", ".codex") } else { ("CLAUDE_CONFIG_DIR", ".claude") };
        // The vendor's own directory: the one its variable names, and the
        // one in the home directory, which is still the person's own login
        // for every tool started without that variable.
        let from_var = Some(self.env(var)).filter(|d| !d.trim().is_empty()).map(PathBuf::from);
        let in_home = Some(self.env("HOME")).filter(|h| !h.trim().is_empty()).map(|h| PathBuf::from(h).join(dot));
        let named = if r.kind == "codex-app-server" { "~/.codex" } else { "~/.claude" };
        match &b.config_dir {
            // No directory of its own is always the person's own: with no
            // HOME to name it, the vendor finds it anyway.
            None => Some(from_var.or(in_home).unwrap_or_else(|| PathBuf::from(named))),
            Some(d) => [from_var, in_home].into_iter().flatten().find(|own| same_dir(d, own)),
        }
    }

    /// What a picker calls an instance: its kind in plain words, and that
    /// it is the person's own login where it is.
    fn label(&self, r: &Resolved) -> String {
        match (self.own_home(r), r.kind) {
            (Some(_), "codex-app-server") => format!("{}, your own Codex login", instances::kind_label(r.kind)),
            (Some(_), _) => format!("{}, your own Claude Code login", instances::kind_label(r.kind)),
            (None, k) => instances::kind_label(k).to_string(),
        }
    }

    /// Whether an instance can run a turn here — the readiness check.
    pub fn status(&self, instance: &str) -> Result<Report, EngineError> {
        let reg = Registry::resolve(&self.definitions()?, self.env);
        let r = reg.get(instance).map_err(|e| EngineError::new("no_instance", e))?;
        Ok(readiness::check(r, &self.credentials, &self.probe()?))
    }

    /// What `krowk connect [target]` connects: a vendor, its method given or
    /// asked; an instance that exists — defined or built in — renewed the
    /// way it was connected; or `<provider>:<name>`, a new one. With no
    /// target, the vendor is asked too.
    pub fn request(&self, target: Option<&str>, method: Option<Method>, options: Options, ui: &mut dyn AuthInteraction) -> Result<Request, EngineError> {
        let target = target.map(str::trim).filter(|t| !t.is_empty());
        let vendor = match target {
            None => {
                let labels: Vec<&str> = Vendor::ALL.iter().map(|v| v.label()).collect();
                Some(Vendor::ALL[choose(ui, "Connect which provider?", &labels, "a vendor: `krowk connect anthropic --method subscription` (or openai, xai, openrouter, openai-compatible)")?])
            }
            Some(t) => Vendor::parse(t),
        };
        if let Some(vendor) = vendor {
            let way = pick(vendor, method, ui)?;
            // Asked which account only when nothing on the command line
            // said: a vendor alone is its default-named account, and a
            // second one is always added by name.
            let instance = match method.is_none() && options.name.is_none() && ui.interactive() {
                true => self.pick_account(way, ui)?,
                false => None,
            };
            return Ok(Request { method: way, instance, options, default: MakeDefault::IfUnset });
        }
        let t = target.expect("a target when no vendor was picked");
        let known = Registry::resolve(&self.definitions()?, self.env);
        let found = match known.instances.get(t) {
            Some(r) => by_kind(r.kind).map(|m| (m, t.to_string())),
            None => t.split_once(':').and_then(|(p, _)| by_provider(p, false)).map(|m| (m, t.to_string())),
        };
        let Some((way, instance)) = found else {
            return Err(EngineError::new(
                "bad_argument",
                format!("{t:?} is not a provider or an instance — connect one of {}, or an instance `krowk status` lists", Vendor::ALL.map(Vendor::id).join(", ")),
            ));
        };
        if options.name.is_some() {
            return Err(bad_flag(format!("{instance} is already the instance's whole name — drop --name, or add an account: `krowk connect {} --method {} --name <new>`", way.vendor.id(), way.method.id())));
        }
        // The device code is the one other way into the same account.
        let way = match method {
            None => way,
            Some(m) if m == way.method => way,
            Some(m) => way.vendor.methods().into_iter().find(|o| o.method == m && o.provider == way.provider).ok_or_else(|| {
                bad_flag(format!("{instance} connects by {} — --method {} would be another instance: `krowk connect {} --method {}`", way.method.id(), m.id(), way.vendor.id(), m.id()))
            })?,
        };
        Ok(Request { method: way, instance: Some(instance), options, default: MakeDefault::IfUnset })
    }

    /// The accounts of this way in there are — defined or built in — each
    /// with its readiness, to reconnect, and a new one, named here. None
    /// when there are none yet: the default-named one is made.
    fn pick_account(&self, way: &'static MethodInfo, ui: &mut dyn AuthInteraction) -> Result<Option<String>, EngineError> {
        let reg = Registry::resolve(&self.definitions()?, self.env);
        let mine: Vec<&Resolved> = reg.instances.values().filter(|r| r.kind == way.kind).collect();
        if mine.is_empty() {
            return Ok(None);
        }
        let reports = readiness::check_all(&mine, &self.credentials, &self.probe()?);
        let mut labels: Vec<String> = reports.iter().zip(&mine).map(|(r, i)| format!("{} ({}) — {}, reconnect", r.instance, self.label(i), r.readiness.label())).collect();
        labels.push("+ new account…".into());
        let options: Vec<&str> = labels.iter().map(String::as_str).collect();
        let at = choose(ui, "Which account?", &options, "--name")?;
        if let Some(r) = reports.get(at) {
            return Ok(Some(r.instance.clone()));
        }
        let name = ask_text(ui, "Name the new account, e.g. work", "--name, e.g. --name work")?;
        let instance = new_instance(way.provider, &name)?;
        if let Some(k) = reg.instances.keys().find(|k| k.to_lowercase() == instance.to_lowercase()) {
            return Err(bad_flag(format!("{k} is connected already — pick it to reconnect it, or give the new account another name")));
        }
        Ok(Some(instance))
    }

    /// The instance `krowk disconnect` signs out: the one named, else one
    /// picked. Never a guess: with nobody to ask, the instances are listed.
    pub fn disconnect_target(&self, target: Option<&str>, ui: &mut dyn AuthInteraction) -> Result<String, EngineError> {
        if let Some(t) = target.map(str::trim).filter(|t| !t.is_empty()) {
            return Ok(t.to_string());
        }
        let reg = Registry::resolve(&self.definitions()?, self.env);
        let names: Vec<&str> = reg.instances.keys().map(String::as_str).collect();
        if !ui.interactive() {
            return Err(EngineError::new("bad_argument", format!("nobody is at a terminal to pick which instance to disconnect, and this host has {} — name one: `krowk disconnect <instance>`", names.join(", "))));
        }
        let labels: Vec<String> = reg.instances.values().map(|r| format!("{} ({})", r.name, self.label(r))).collect();
        let options: Vec<&str> = labels.iter().map(String::as_str).collect();
        Ok(names[choose(ui, "Disconnect which instance?", &options, "the instance, e.g. `krowk disconnect claude:work`")?].to_string())
    }

    /// Connects: writes the definition — after the sign-in, which must
    /// succeed first — or renews the login of the one there is.
    pub fn connect(&self, req: &Request, ui: &mut dyn AuthInteraction) -> Result<Connected, EngineError> {
        let way = req.method;
        let o = &req.options;
        let provider = way.provider;
        let name = clean(&o.name);
        let instance = match (&req.instance, &name, provider) {
            (Some(i), _, _) => i.clone(),
            (None, Some(n), p) => new_instance(p, n)?,
            (None, None, "openai-compatible") => new_instance(provider, &ask_text(ui, "Name this server's instance (the part of --model before the model id), e.g. local", "--name, e.g. --name local")?)?,
            (None, None, p) => p.to_string(),
        };
        check_name(&instance)?;
        let backend = matches!(provider, "claude" | "codex");
        // A directory given relative to where krowk runs is kept as the
        // absolute one it means: the vendor's login runs elsewhere, and a
        // turn resolves a definition's path against its own project.
        // A `~` the shell left alone (`--config-dir=~/acct`, a quoted one) is
        // the home directory, never a directory named `~` here.
        let home = self.env("HOME");
        let config_dir = match clean(&o.config_dir).map(|d| match d.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => match home.trim() {
                "" => Err(bad_flag(format!("--config-dir {d}: HOME is not set, so `~` names nothing — give the whole path"))),
                h => Ok(format!("{h}{rest}")),
            },
            _ => Ok(d),
        }) {
            Some(d) => Some(normalised(&std::path::absolute(&d?).map_err(|e| bad_flag(format!("--config-dir: {e}")))?).display().to_string()),
            None => None,
        };
        if !backend && (o.binary.is_some() || o.config_dir.is_some()) {
            return Err(bad_flag("`--binary` and `--config-dir` describe a Claude Code or Codex account — `krowk connect anthropic --method subscription --name work --config-dir …`, or openai"));
        }
        let existing = self.definitions()?;
        let renewed = existing.instances.contains_key(&instance);
        // Every instance there is, built in or defined: a new one never
        // takes a built-in's name for another kind, nor one that differs from
        // an existing name only in case — the directories made from the two
        // are one directory on a disk that ignores case.
        let known = Registry::resolve(&existing, self.env);
        match known.instances.get(&instance) {
            Some(r) if r.kind != way.kind => {
                return Err(EngineError::new("instance_exists", format!("{instance} is already {} ({}) — give this one another --name", r.kind, instances::kind_label(r.kind))));
            }
            Some(_) => {}
            None => {
                if req.instance.is_some() {
                    // `krowk connect claude:work` of one not made yet.
                    let rest = instance.strip_prefix(provider).and_then(|r| r.strip_prefix(':')).unwrap_or(&instance);
                    new_instance(provider, rest)?;
                }
                if let Some(k) = known.instances.keys().find(|k| k.to_lowercase() == instance.to_lowercase()) {
                    return Err(bad_flag(format!("{instance} differs from {k}, which is there already, only in case — reconnect {k}, or give this one another --name")));
                }
            }
        }
        // The name after the provider: what a new profile's variable and
        // directory are made from.
        let named = instance.strip_prefix(provider).and_then(|r| r.strip_prefix(':')).map(String::from).or_else(|| (instance != provider).then(|| instance.clone()));
        // A new named profile of a provider reads its own variable, so two
        // never share a key by accident; one that exists keeps its own.
        let key_env = clean(&o.api_key_env).or_else(|| match (&named, provider) {
            _ if renewed => None,
            (_, "supergrok" | "claude" | "codex" | "openai-compatible") => None,
            (Some(n), p) => Some(format!("{}_{}_API_KEY", env_part(p), env_part(n))),
            (None, _) => None,
        });
        // A derived variable is one name for `my-work`, `my_work` and
        // `MY_WORK`: two instances would share a key by accident.
        if clean(&o.api_key_env).is_none()
            && let Some(var) = &key_env
            && let Some(other) = known.instances.values().find(|r| r.name != instance && r.api_key_env == *var)
        {
            return Err(bad_flag(format!("{instance} would read its key from ${var}, which {} reads already — name another with --api-key-env", other.name)));
        }
        let base_url = clean(&o.base_url);
        let base_url = match (provider, base_url) {
            ("openai-compatible", None) if !existing.instances.contains_key(&instance) => Some(ask_text(ui, "The server's base URL, e.g. http://127.0.0.1:11434/v1", "--base-url, e.g. --base-url http://127.0.0.1:11434/v1")?).filter(|u| !u.is_empty()),
            (_, b) => b,
        };
        let base_given = base_url.is_some();
        let kind = match provider {
            "anthropic" => InstanceKind::AnthropicApi { api_key_env: key_env, base_url, thinking: None, max_tokens: None, effort: None },
            "openai" => InstanceKind::OpenaiApi { api_key_env: key_env, base_url, wire_api: None, effort: None },
            "xai" => InstanceKind::XaiApi { api_key_env: key_env, base_url, effort: None },
            "openrouter" => InstanceKind::OpenrouterApi { api_key_env: key_env, base_url, effort: None },
            // An existing server keeps its base URL: a placeholder here is
            // replaced by the merge below.
            "openai-compatible" => InstanceKind::OpenaiCompatible {
                base_url: base_url.clone().or_else(|| renewed.then(String::new)).ok_or_else(|| bad_flag("openai-compatible needs --base-url, e.g. http://127.0.0.1:11434/v1"))?,
                api_key_env: key_env,
                provider: None,
                wire_api: None,
                effort: None,
            },
            // A router in front of Claude Code: the base URL it sends to goes
            // in the instance's env, and the key is named by --api-key-env —
            // krowk reads that variable and hands the key to the process
            // (ANTHROPIC_AUTH_TOKEN with a base URL, ANTHROPIC_API_KEY
            // without). Claude Code inherits neither from krowk's own
            // environment.
            "claude" => InstanceKind::ClaudeCode {
                binary: clean(&o.binary),
                config_dir: config_dir.clone(),
                env: base_url.clone().map(|u| [("ANTHROPIC_BASE_URL".to_string(), u)].into()).unwrap_or_default(),
                args: Vec::new(),
                api_key_env: key_env,
                effort: None,
            },
            // A router in front of Codex is a model provider in its args; the
            // base URL goes in its env as OPENAI_BASE_URL, and the key is
            // named by --api-key-env — krowk reads that variable and hands
            // the key to the process under the same name.
            "codex" => InstanceKind::CodexAppServer {
                binary: clean(&o.binary),
                codex_home: config_dir.clone(),
                env: base_url.clone().map(|u| [("OPENAI_BASE_URL".to_string(), u)].into()).unwrap_or_default(),
                args: Vec::new(),
                api_key_env: key_env,
                effort: None,
            },
            _ => InstanceKind::XaiOauth { base_url, issuer: None, client_id: clean(&o.client_id), scope: None, effort: None },
        };
        // One that exists is updated: the fields given replace its own, and
        // the rest — an issuer, a pinned wire API — are kept.
        let kind = match existing.instances.get(&instance) {
            Some(old) if old.tag() != kind.tag() => {
                return Err(EngineError::new("instance_exists", format!("{instance} is already defined as {} — disconnect it first with `krowk disconnect {instance} --remove`", old.tag())));
            }
            Some(old) => {
                let mut merged = serde_json::to_value(old).expect("a definition serializes");
                let mut new = serde_json::to_value(&kind).expect("a definition serializes");
                if let Some(n) = new.as_object_mut().filter(|_| !base_given) {
                    n.remove("baseUrl");
                }
                if let (Some(m), Value::Object(new)) = (merged.as_object_mut(), new) {
                    for (k, v) in new {
                        // `env` is merged by key: a base URL given does not
                        // take the definition's other variables with it.
                        match (m.get_mut(&k), v) {
                            (Some(Value::Object(old)), Value::Object(v)) if k == "env" => old.extend(v),
                            (_, v) => {
                                m.insert(k, v);
                            }
                        }
                    }
                }
                serde_json::from_value(merged).map_err(|e| EngineError::new("bad_config", format!("{instance}: {e}")))?
            }
            None => kind,
        };

        // A new named account gets a directory of its own under krowk's
        // data directory; the unnamed one is the vendor as the person
        // already uses it.
        let account = |vendor: &str| -> Result<Option<String>, EngineError> {
            Ok(match renewed || named.is_none() {
                true => None,
                false => Some(self.data_dir()?.join(vendor).join(instance.replace(':', "-")).display().to_string()),
            })
        };
        let mut vendor = None;
        let kind = match kind {
            InstanceKind::ClaudeCode { binary, config_dir, env, args, api_key_env, effort } => {
                let config_dir = match config_dir {
                    Some(d) => Some(d),
                    None => account("claude")?,
                };
                if !renewed || o.config_dir.is_some() {
                    self.dir_free(&instance, config_dir.as_deref(), &known, ("CLAUDE_CONFIG_DIR", ".claude"))?;
                }
                let kind = InstanceKind::ClaudeCode { binary, config_dir, env, args, api_key_env, effort };
                vendor = Some(self.sign_in_claude(&instance, &kind, ui)?);
                kind
            }
            InstanceKind::CodexAppServer { binary, codex_home, env, args, api_key_env, effort } => {
                let codex_home = match codex_home {
                    Some(d) => Some(d),
                    None => account("codex")?,
                };
                if !renewed || o.config_dir.is_some() {
                    self.dir_free(&instance, codex_home.as_deref(), &known, ("CODEX_HOME", ".codex"))?;
                }
                let kind = InstanceKind::CodexAppServer { binary, codex_home, env, args, api_key_env, effort };
                vendor = Some(self.sign_in_codex(&instance, &kind, way.method == Method::Device, ui)?);
                kind
            }
            k => k,
        };
        // SuperGrok signs in first too: a login that fails leaves nothing.
        let oauth = matches!(kind, InstanceKind::XaiOauth { .. });
        if oauth {
            let Auth::OAuth { issuer, client_id, scope } = self.resolve(&instance, &kind)?.auth else { unreachable!("an xai-oauth instance signs in with OAuth") };
            let stored = oauth::sign_in(&oauth::Login { issuer, client_id, scope }, way.method == Method::Device, &mut |step| match step {
                Step::Open(url) => ui.notify(Notice::AuthUrl { url, message: "Sign in to xAI with your SuperGrok or X Premium account:" }),
                Step::Device(p) => ui.notify(Notice::DeviceCode {
                    url: p.verification_uri_complete.as_deref().unwrap_or(&p.verification_uri),
                    code: &p.user_code,
                    message: "to sign in to xAI with your SuperGrok or X Premium account",
                }),
            })?;
            Store::new(self.credentials.clone()).save(&instance, &stored).map_err(|e| EngineError::new("credentials_unwritable", e.message))?;
        }
        self.write(&instance, Some(&kind))?;
        readiness::forget(&instance);
        // The first connection is the default: `defaultModel` is set when
        // config.json has none, and replaced only when asked.
        let unset = !self.raw_config()?.get("defaultModel").and_then(Value::as_str).is_some_and(|m| !m.trim().is_empty());
        let wanted = req.default == MakeDefault::Always || (req.default == MakeDefault::IfUnset && unset);
        let default_model = default_model(kind.tag()).filter(|_| wanted).map(|m| format!("{instance}/{m}"));
        if let Some(m) = &default_model {
            self.edit(|raw| {
                raw.insert("defaultModel".into(), Value::String(m.clone()));
            })?;
        }
        let resolved = self.resolve(&instance, &kind)?;
        Ok(Connected { instance, definition: kind, resolved, renewed, oauth, vendor, default_model })
    }

    /// An account's directory is its own: one another definition already
    /// signs in in — by the same path, or one differing only in case —
    /// would share that login.
    ///
    /// Every instance counts, the built-ins' own `~/.claude` and `~/.codex`
    /// too, and paths are compared by where they really lead.
    ///
    /// The vendor's own directories — the one its variable names and the one
    /// in the home directory (`own`) — are the person's own login whichever
    /// the built-in runs on, so neither is ever a new account's.
    fn dir_free(&self, instance: &str, dir: Option<&str>, known: &Registry, own: (&str, &str)) -> Result<(), EngineError> {
        let Some(dir) = dir else { return Ok(()) };
        let taken = known.instances.values().find(|r| r.name != instance && r.backend.as_ref().and_then(|b| b.home.as_deref()).is_some_and(|h| same_dir(h, Path::new(dir))));
        if let Some(other) = taken {
            return Err(bad_flag(format!("{dir} is {}'s directory already — one login cannot be two accounts; give this one another --name or --config-dir", other.name)));
        }
        let (var, dot) = own;
        let from_var = Some(self.env(var)).filter(|d| !d.trim().is_empty()).map(PathBuf::from);
        let in_home = Some(self.env("HOME")).filter(|h| !h.trim().is_empty()).map(|h| PathBuf::from(h).join(dot));
        if !known.instances.contains_key(instance) && [from_var, in_home].into_iter().flatten().any(|o| same_dir(&o, Path::new(dir))) {
            return Err(bad_flag(format!("{dir} is your own login's directory — one login cannot be two accounts; give this one another --config-dir")));
        }
        Ok(())
    }

    /// R-INST-2: a Claude Code account is signed in by Claude Code. Its
    /// config directory is made (0700) if it is new, `claude auth status` is
    /// asked, and when it says no, `claude auth login` runs on the real
    /// terminal in Anthropic's own flow; then status is asked again. A keyed
    /// instance — a router (a base URL in its env) or a Console key — runs
    /// on that key, which krowk reads and hands the process, so no login is
    /// run for it. A failure removes a directory this made.
    fn sign_in_claude(&self, instance: &str, kind: &InstanceKind, ui: &mut dyn AuthInteraction) -> Result<VendorLogin, EngineError> {
        let backend = self.resolve(instance, kind)?.backend.expect("a claude-code instance has a backend");
        if backend.path.is_none() {
            return Err(EngineError::new("backend_not_found", format!("{} was not found — install Claude Code (https://claude.com/claude-code), or name the binary with --binary", backend.binary)));
        }
        absolute_home(instance, &backend)?;
        let probe = self.probe()?;
        let made = made_dir(backend.config_dir.as_deref())?;
        let undo = |e: EngineError| undo(&made, e);
        let status = claude_auth::status(&backend, &probe).map_err(|e| undo(backend_failed(e)))?;
        if status.logged_in || backend.env.contains_key("ANTHROPIC_BASE_URL") || backend.key.is_some() {
            return Ok(VendorLogin { logged_in: status.logged_in, describe: status.describe(), ran: false, shared: Vec::new() });
        }
        let kept = backend.config_dir.as_ref().map(|d| format!(", kept in {}", d.display())).unwrap_or_default();
        ui.notify(Notice::Info(&format!("Signing {instance} in to Claude Code — what follows is Claude's own login (`claude auth login`){kept}:")));
        let exit = ui.terminal(&mut || claude_auth::login(&backend, &probe.dir)).map_err(|e| undo(backend_failed(e)))?;
        let status = claude_auth::status(&backend, &probe).map_err(|e| undo(backend_failed(e)))?;
        if !status.logged_in {
            return Err(undo(not_signed_in("claude auth login", exit, instance, kind)));
        }
        Ok(VendorLogin { logged_in: true, describe: status.describe(), ran: true, shared: Vec::new() })
    }

    /// R-INST-2, with `codex login`: a Codex account is signed in by Codex.
    /// Its home is made (0700) if it is new and given links to the person's
    /// own Codex configuration (`codex::share`), Codex is asked whether it is
    /// signed in (`account/read`, else `codex login status`), and when it
    /// says no, `codex login` runs on the real terminal in OpenAI's own flow
    /// — `--device-auth` for the device code; then status is asked again. A
    /// keyed instance — a router — runs on its key, so no login is run for
    /// it. A failure removes a home this made.
    fn sign_in_codex(&self, instance: &str, kind: &InstanceKind, device: bool, ui: &mut dyn AuthInteraction) -> Result<VendorLogin, EngineError> {
        let backend = self.resolve(instance, kind)?.backend.expect("a codex instance has a backend");
        if backend.path.is_none() {
            return Err(EngineError::new("backend_not_found", format!("{} was not found — install Codex (https://developers.openai.com/codex), or name the binary with --binary", backend.binary)));
        }
        absolute_home(instance, &backend)?;
        let probe = self.probe()?;
        let made = made_dir(backend.config_dir.as_deref())?;
        let undo = |e: EngineError| undo(&made, e);
        // The person's own Codex home: what a new account's home shares.
        let own = Some(self.env("CODEX_HOME")).filter(|d| !d.trim().is_empty()).map(PathBuf::from).or_else(|| Some(self.env("HOME")).filter(|h| !h.trim().is_empty()).map(|h| PathBuf::from(h).join(".codex")));
        let share = || match (&backend.config_dir, &own) {
            (Some(dir), Some(own)) => codex::share(dir, own).map_err(|e| EngineError::new("config_unwritable", format!("link {} into {}: {e}", own.display(), dir.display()))),
            _ => Ok(Vec::new()),
        };
        // Links go into a home this made at once, since a failure takes the
        // whole of it away; into one that was there, only once the sign-in
        // worked, so a failed one leaves it as it was.
        let mut shared = if made.is_some() { share().map_err(undo)? } else { Vec::new() };
        let status = codex_auth::signed_in(&backend, &probe).map_err(|e| undo(backend_failed(e)))?;
        if status.logged_in || backend.key.is_some() {
            if made.is_none() {
                shared = share()?;
            }
            return Ok(VendorLogin { logged_in: status.logged_in, describe: status.describe(), ran: false, shared });
        }
        let kept = backend.config_dir.as_ref().map(|d| format!(", kept in {}", d.display())).unwrap_or_default();
        ui.notify(Notice::Info(&format!("Signing {instance} in to Codex — what follows is Codex's own login (`codex login`){kept}:")));
        let exit = ui.terminal(&mut || codex_auth::login(&backend, device, &probe.dir)).map_err(|e| undo(backend_failed(e)))?;
        let status = codex_auth::signed_in(&backend, &probe).map_err(|e| undo(backend_failed(e)))?;
        if !status.logged_in {
            return Err(undo(not_signed_in("codex login", exit, instance, kind)));
        }
        if made.is_none() {
            shared = share()?;
        }
        Ok(VendorLogin { logged_in: true, describe: status.describe(), ran: true, shared })
    }

    /// Signs an instance out, the way its kind is signed in: SuperGrok's
    /// tokens are deleted from krowk's credentials file; a Claude Code or
    /// Codex account runs the vendor's own sign-out (`claude auth logout`,
    /// `codex logout`) with its own directory; an API key is the
    /// environment's, which krowk only reads, so the variable is named. The
    /// definition stays unless `remove`, and is taken away only once the
    /// sign-out worked.
    ///
    /// **The person's own vendor login is not krowk's to sign out of
    /// quietly.** The built-in `claude` and `codex` run on `~/.claude` and
    /// `~/.codex`, which Claude Code and Codex themselves, and every other
    /// tool on the machine, use: signing them out there signs the person
    /// out everywhere. So it is said plainly and asked at a terminal, and
    /// without one refused unless `own_login` (`--sign-out-vendor`) says
    /// that is what is wanted.
    pub fn disconnect(&self, instance: &str, remove: bool, own_login: bool, ui: &mut dyn AuthInteraction) -> Result<Disconnected, EngineError> {
        let defs = self.definitions()?;
        let reg = Registry::resolve(&defs, self.env);
        let r = reg.instances.get(instance).ok_or_else(|| EngineError::new("no_instance", format!("no instance named {instance} — `krowk status` lists them")))?;
        let signed_out = match (&r.auth, &r.backend) {
            (Auth::OAuth { .. }, _) => SignedOut::Tokens { had: Store::new(self.credentials.clone()).remove(instance).map_err(|e| EngineError::new("credentials_unwritable", e.message))? },
            (Auth::Vendor, Some(b)) if b.key.is_some() => SignedOut::Key { var: r.api_key_env.clone() },
            (Auth::Vendor, Some(b)) => {
                let codex = r.kind == "codex-app-server";
                let (command, var) = if codex { ("codex logout", "CODEX_HOME") } else { ("claude auth logout", "CLAUDE_CONFIG_DIR") };
                if b.path.is_none() {
                    return Err(EngineError::new("backend_not_found", format!("{} was not found, so `{command}` cannot run for {instance}", b.binary)));
                }
                if let Some(home) = self.own_home(r).filter(|_| !own_login) {
                    let who = if codex { "Codex" } else { "Claude Code" };
                    let warning = format!("disconnecting {instance} signs you out of {who} itself, in {}, for every tool that uses it", home.display());
                    if !ui.interactive() {
                        return Err(EngineError::new("confirmation_required", format!("{warning} — to do that, run `krowk disconnect {instance} --sign-out-vendor`")));
                    }
                    let options = ["No — keep that login", "Yes — sign me out of it"];
                    if choose(ui, &format!("{}. Go on?", capitalised(&warning)), &options, "--sign-out-vendor")? != 1 {
                        return Err(EngineError::new("selection_cancelled", format!("nothing was signed out — {instance} is as it was")));
                    }
                }
                absolute_home(instance, b)?;
                let probe = self.probe()?;
                let exit = if codex { codex_auth::logout(b, &probe) } else { claude_auth::logout(b, &probe) }.map_err(backend_failed)?;
                if !exit.success() {
                    let at = b.home.as_ref().map(|h| format!("{var}={} ", h.display())).unwrap_or_default();
                    return Err(backend_failed(format!("`{command}` stopped ({exit}) for {instance} — run `{at}{command}` to see why")));
                }
                SignedOut::Vendor { command, home: b.home.clone() }
            }
            (Auth::ApiKey, _) => SignedOut::Key { var: r.api_key_env.clone() },
            _ => SignedOut::Keyless,
        };
        readiness::forget(instance);
        let removed_definition = remove && defs.instances.contains_key(instance);
        let mut cleared_default = None;
        if removed_definition {
            self.write(instance, None)?;
            // A default that ran on it would name an instance that is gone.
            let default = self.raw_config()?.get("defaultModel").and_then(Value::as_str).map(String::from);
            // A built-in's name still resolves once its definition is gone.
            let built_in = instances::implicit().iter().any(|(n, _)| *n == instance);
            if let Some(m) = default.filter(|m| !built_in && m.split_once('/').is_some_and(|(i, _)| i == instance)) {
                self.edit(|raw| {
                    raw.remove("defaultModel");
                })?;
                cleared_default = Some(m);
            }
        }
        Ok(Disconnected { instance: instance.into(), kind: r.kind, signed_out, removed_definition, cleared_default })
    }

    /// Takes a definition, and krowk's own login for it, away — `krowk
    /// providers remove`. A Claude Code or Codex account's directory holds
    /// the vendor's own login, which is the vendor's to sign out of
    /// (`disconnect`): it is kept.
    pub fn remove(&self, instance: &str) -> Result<Removed, EngineError> {
        let defs = self.definitions()?;
        let definition = defs.instances.contains_key(instance);
        if definition {
            self.write(instance, None)?;
        }
        let login = Store::new(self.credentials.clone()).remove(instance).map_err(|e| EngineError::new("credentials_unwritable", e.message))?;
        if !definition && !login {
            let implicit = instances::implicit().iter().any(|(n, _)| *n == instance);
            let why = if implicit { format!("{instance} is built in and has no definition or login to remove") } else { format!("no instance named {instance} is defined — `krowk providers list` shows them") };
            return Err(EngineError::new("no_instance", why));
        }
        readiness::forget(instance);
        let kept = match defs.instances.get(instance) {
            Some(InstanceKind::ClaudeCode { config_dir: Some(dir), .. }) => Some((dir.clone(), false)),
            Some(InstanceKind::CodexAppServer { codex_home: Some(dir), .. }) => Some((dir.clone(), true)),
            _ => None,
        };
        Ok(Removed { definition, login, kept })
    }
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The way in, given or asked: a vendor with one way in takes it.
fn pick(vendor: Vendor, method: Option<Method>, ui: &mut dyn AuthInteraction) -> Result<&'static MethodInfo, EngineError> {
    let offered = vendor.methods();
    let ids = offered.iter().map(|m| m.method.id()).collect::<Vec<_>>();
    if let Some(m) = method {
        return offered.iter().find(|o| o.method == m).copied().ok_or_else(|| bad_flag(format!("{} connects by {} — not {}", vendor.id(), ids.join(" or "), m.id())));
    }
    if let [only] = offered.as_slice() {
        return Ok(only);
    }
    let labels = offered.iter().map(|m| m.label).collect::<Vec<_>>();
    let flag = format!("--method {}", ids.join("|"));
    Ok(offered[choose(ui, &format!("How do you connect {}?", vendor.id()), &labels, &flag)?])
}

/// What a sign-in made: the account's directory, and the outermost of its
/// parents that was missing and made with it.
struct Made {
    leaf: PathBuf,
    top: PathBuf,
}

/// The directory a sign-in makes, when it is new — taken away again if the
/// sign-in fails. Only a path that is absolute and lexically normal is made
/// here — what `connect` writes, `--config-dir` included; a hand-written
/// relative or `..` one is left for the vendor to make, from the path it is
/// given, rather than made here somewhere else.
/// A vendor's login and sign-out run in krowk's own directory, so a
/// hand-written relative directory would be one there, not the one turns
/// use: refused, with what to write instead.
fn absolute_home(instance: &str, b: &instances::Backend) -> Result<(), EngineError> {
    match &b.config_dir {
        Some(d) if !d.is_absolute() => Err(EngineError::new("bad_config", format!("{instance}'s config directory {} is relative — write it as an absolute path in config.json, then run this again", d.display()))),
        _ => Ok(()),
    }
}

fn made_dir(dir: Option<&Path>) -> Result<Option<Made>, EngineError> {
    let Some(d) = dir.filter(|d| d.is_absolute() && normalised(d) == *d && d.symlink_metadata().is_err()) else { return Ok(None) };
    let top = d.ancestors().take_while(|a| a.symlink_metadata().is_err()).last().unwrap_or(d).to_path_buf();
    let failed = |e: std::io::Error| EngineError::new("config_unwritable", format!("create {}: {e}", d.display()));
    if let Some(parent) = d.parent() {
        private_dir(parent).map_err(failed)?;
    }
    // The account's own directory is made alone, not recursively: one that
    // appeared since it was looked for (another connect of the same name)
    // is not this one's, and a failure must never take it away.
    #[cfg(unix)]
    let leaf = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(d)
    };
    #[cfg(not(unix))]
    let leaf = std::fs::create_dir(d);
    match leaf {
        Ok(()) => Ok(Some(Made { leaf: d.to_path_buf(), top })),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(failed(e)),
    }
}

/// `.` and `..` taken out of a path by its words alone, as a person reads
/// it — `/a/new/../Other` is `/a/Other` — so a directory made or removed is
/// the one named, never one a `..` climbs into.
fn normalised(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Where a path really leads, for comparing two: the nearest ancestor that
/// exists, resolved (symlinks and all), with the rest of the words after it.
fn real(p: &Path) -> PathBuf {
    let p = normalised(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));
    let mut rest = Vec::new();
    let mut at = p.as_path();
    loop {
        if let Ok(c) = at.canonicalize() {
            return rest.iter().rev().fold(c, |acc: PathBuf, r| acc.join(r));
        }
        match (at.file_name(), at.parent()) {
            (Some(n), Some(up)) => {
                rest.push(n.to_os_string());
                at = up;
            }
            _ => return p,
        }
    }
}

/// Whether two paths are one directory. Case is folded only where the disk
/// ignores it: macOS and Windows, by default.
fn same_dir(a: &Path, b: &Path) -> bool {
    let (a, b) = (real(a), real(b));
    if cfg!(any(target_os = "macos", windows)) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Takes away what a failed sign-in made: the account's own directory,
/// whole, then each parent it made, only while it is empty — another
/// connection may have put its own account beside this one meanwhile.
fn undo(made: &Option<Made>, e: EngineError) -> EngineError {
    if let Some(m) = made {
        let _ = std::fs::remove_dir_all(&m.leaf);
        if m.leaf != m.top {
            for up in m.leaf.ancestors().skip(1) {
                if std::fs::remove_dir(up).is_err() || up == m.top {
                    break;
                }
            }
        }
    }
    e
}

fn not_signed_in(command: &str, exit: ExitStatus, instance: &str, kind: &InstanceKind) -> EngineError {
    let how = if exit.success() { "finished without signing in".to_string() } else { format!("stopped ({exit})") };
    EngineError::new("not_authenticated", format!("`{command}` {how}, so {instance} was not connected and nothing was written — run `{}` to try again", connect_command(instance, kind.tag())))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fix line is a command a person pastes, so each one must name the
    // instance it was written for — checked here, where every kind and
    // every shape of name is cheap to walk.
    #[test]
    fn a_fix_line_connects_the_instance_it_names() {
        for (instance, kind, want) in [
            ("claude", "claude-code", "krowk connect anthropic --method subscription"),
            ("claude:work", "claude-code", "krowk connect anthropic --method subscription --name work"),
            ("codex:team", "codex-app-server", "krowk connect openai --method subscription --name team"),
            ("supergrok", "xai-oauth", "krowk connect xai --method subscription"),
            ("grok:team", "xai-oauth", "krowk connect grok:team"),
            ("anthropic:work", "anthropic-api", "krowk connect anthropic --method api-key --name work"),
            ("openrouter", "openrouter-api", "krowk connect openrouter"),
            ("local", "openai-compatible", "krowk connect openai-compatible --name local"),
            ("mywork", "claude-code", "krowk connect mywork"),
        ] {
            assert_eq!(connect_command(instance, kind), want, "{instance}");
        }
    }

    #[test]
    fn every_way_in_is_the_vendor_and_method_its_kind_names() {
        for m in METHODS.iter().filter(|m| m.method != Method::Device) {
            assert_eq!(instances::kind_connect(m.kind), Some((m.vendor.id(), m.method.id())), "{}", m.kind);
        }
    }

    #[test]
    fn a_new_name_keeps_its_method_prefix_and_no_colon_of_its_own() {
        assert_eq!(normalised(Path::new("/a/new/../Other/./x")), Path::new("/a/Other/x"));
        assert_eq!(new_instance("claude", "work").unwrap(), "claude:work");
        assert_eq!(new_instance("claude", "claude:work").unwrap(), "claude:work");
        assert_eq!(new_instance("openai-compatible", "local").unwrap(), "local");
        for (p, n) in [("claude", "claude:a:b"), ("anthropic", "codex:x"), ("openai-compatible", "claude:x"), ("claude", "a/b"), ("claude", "..")] {
            assert!(new_instance(p, n).is_err(), "{p} {n}");
        }
    }
}
