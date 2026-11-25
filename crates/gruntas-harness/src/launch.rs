//! The launcher's plans (Launcher D1–D5): how `gruntas run <harness>` starts
//! the stock `claude` or `codex` on one account, attended (the harness a
//! child of gruntas at the terminal, `crate::attended`) or unattended (a
//! background session).
//! A plan is the whole launch as data: the program, the environment (the
//! account's config home), the arguments with the gruntas plugin and, for
//! Claude Code, one `--settings` JSON carrying the sandbox parts that keep
//! the session out of the gruntas home and the worktree hooks, merged with
//! any `--settings` the person passed; for Codex, a permissions profile
//! that reads everything but the gruntas home.
//!
//! Pure: a plan resolves paths and reads a settings file the person named,
//! and writes nothing, an account's settings, credentials and installed
//! plugins least of all (R-RUN-5). Running it is the CLI's.

use crate::instances::{Backend, Registry, Resolved};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The two harnesses gruntas launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub fn parse(word: &str) -> Result<Harness, String> {
        match word {
            "claude" => Ok(Harness::Claude),
            "codex" => Ok(Harness::Codex),
            other => Err(format!("gruntas runs claude or codex, not {other:?} — `gruntas run claude` or `gruntas run codex`")),
        }
    }

    /// Its name, which is also its default account's instance and the
    /// prefix of every other (`claude:work`).
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// The instance kind its accounts are.
    pub fn kind(self) -> &'static str {
        match self {
            Harness::Claude => "claude-code",
            Harness::Codex => "codex-app-server",
        }
    }
}

/// What `--account` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// No `--account`, or the harness's own name: the person's own config
    /// home (`claude`, `codex`).
    Default,
    /// `--account work`: the harness's account `work` (`claude:work`).
    Named(String),
    /// `--account auto`: the account with the most headroom (LN05).
    Auto,
}

impl Account {
    /// `--account`'s value for `harness`; empty is no `--account`. The whole
    /// instance name (`claude:work`) is read as its part after the prefix,
    /// and another harness's (`codex:team` for claude) refused.
    pub fn parse(harness: Harness, value: &str) -> Result<Account, String> {
        let v = value.trim();
        let rest = match v.split_once(':') {
            None => v,
            Some((pre, rest)) if pre == harness.name() => rest,
            Some(_) => return Err(format!("{v:?} is not a {} account — give --account the name after `{}:`, e.g. --account work", harness.name(), harness.name())),
        };
        Ok(match rest {
            "" if v.is_empty() => Account::Default,
            "" => return Err(format!("{v:?} names no account — e.g. --account work")),
            r if r == harness.name() => Account::Default,
            "auto" => Account::Auto,
            r => Account::Named(r.to_string()),
        })
    }

    /// The instance it names; none for `auto`, which LN05 picks.
    pub fn instance(&self, harness: Harness) -> Option<String> {
        match self {
            Account::Default => Some(harness.name().to_string()),
            Account::Named(n) => Some(format!("{}:{n}", harness.name())),
            Account::Auto => None,
        }
    }
}

/// The account's instance in `registry`, renamed or not, refused when it is
/// missing or of another kind, with the command that connects one.
pub fn account<'r>(registry: &'r Registry, harness: Harness, name: &str) -> Result<&'r Resolved, String> {
    let (vendor, method) = crate::instances::kind_connect(harness.kind()).unwrap_or_default();
    let connect = match name.split_once(':') {
        Some((_, rest)) => format!("gruntas connect {vendor} --method {method} --name {rest}"),
        None => format!("gruntas connect {vendor} --method {method}"),
    };
    let inst = registry.instances.get(&registry.current(name)).ok_or_else(|| format!("there is no {} account {name:?} — connect it with `{connect}`", harness.name()))?;
    if inst.kind != harness.kind() || inst.backend.is_none() {
        return Err(format!("{name:?} is a {} instance, not a {} account — pick one `gruntas providers list` shows as {}", inst.kind, harness.name(), crate::instances::kind_label(harness.kind())));
    }
    Ok(inst)
}

/// Attended: the harness takes the terminal. Unattended: a background
/// session started on a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Attended,
    Unattended { task: String },
}

/// Everything a plan is made from, resolved by the caller.
pub struct Request<'a> {
    pub harness: Harness,
    pub mode: Mode,
    /// The account's instance (`account`).
    pub instance: &'a Resolved,
    /// Every path the sandbox hides (`hidden`): the gruntas home by its
    /// resolved absolute path first.
    pub hidden: Vec<PathBuf>,
    /// The gruntas plugin's directory (`plugin_dir`).
    pub plugin_dir: PathBuf,
    /// The gruntas binary the worktree hooks run.
    pub gruntas: PathBuf,
    /// False for `--no-sandbox`: the sandbox parts are left out.
    pub sandbox: bool,
    /// The open proxy an attended Claude session's sandboxed commands
    /// reach the network through (`crate::egress`); none leaves Claude
    /// Code's own, which asks before each new domain.
    pub egress: Option<crate::egress::Egress>,
    /// The person's own arguments, passed through but for `--settings`,
    /// which is merged into gruntas's.
    pub args: Vec<String>,
}

/// One launch, as data.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub harness: Harness,
    pub mode: Mode,
    /// What runs: the instance's binary where it was found, else its name.
    pub program: PathBuf,
    /// The account's config home (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); none
    /// is the vendor's own default.
    pub config_home: Option<PathBuf>,
    /// Inherited variables taken away, and those set.
    pub env_remove: Vec<&'static str>,
    pub env_set: Vec<(String, String)>,
    /// Every argument after the program. For Codex unattended, the profile
    /// the engine adds after `app-server` and the instance's own arguments.
    pub args: Vec<String>,
    /// Claude Code's merged `--settings`, also in `args`.
    pub settings: Option<Value>,
    /// Whether the sandbox parts are in.
    pub sandboxed: bool,
}

/// The plan for `req`. An unattended Claude session's task is its prompt,
/// after the person's arguments, so they carry no prompt of their own.
/// `read` reads a `--settings` the person gave as a file. Refused: an unattended session without the sandbox (D4), and a
/// `--settings` that is not a JSON object.
pub fn plan(req: &Request, read: &dyn Fn(&Path) -> std::io::Result<String>) -> Result<Plan, String> {
    if !req.sandbox && matches!(req.mode, Mode::Unattended { .. }) {
        return Err("an unattended session always runs in the sandbox that keeps it out of the gruntas home — drop --no-sandbox".into());
    }
    let b: &Backend = req.instance.backend.as_ref().ok_or_else(|| format!("{} is not a {} account", req.instance.name, req.harness.name()))?;
    let (env_remove, env_set) = match req.harness {
        Harness::Claude => crate::claude::environment(b),
        Harness::Codex => crate::codex::environment(b),
    };
    let mut plan = Plan {
        harness: req.harness,
        mode: req.mode.clone(),
        program: b.path.clone().unwrap_or_else(|| PathBuf::from(&b.binary)),
        config_home: b.config_dir.clone(),
        env_remove,
        env_set,
        args: Vec::new(),
        settings: None,
        sandboxed: req.sandbox,
    };
    // The default account is what `claude` in the person's shell is: never
    // a CLAUDE_CONFIG_DIR set (given `~/.claude` as one, Claude Code reads
    // `~/.claude/.claude.json` rather than `~/.claude.json`, and every
    // workspace the person trusted looks untrusted), an inherited one kept.
    // Its config home is the caller's to fill from that inherited one.
    if req.harness == Harness::Claude && req.instance.name == req.harness.name() {
        plan.config_home = None;
        plan.env_set.retain(|(k, _)| k != "CLAUDE_CONFIG_DIR");
    }
    match req.harness {
        Harness::Claude => {
            // The instance's own arguments' `--settings` too: a second one
            // after gruntas's would replace it, sandbox and all.
            let (mut theirs, configured) = take_settings(&b.args, read)?;
            let (typed, rest) = take_settings(&req.args, read)?;
            theirs.extend(typed);
            let mut ours = worktree_hooks(&req.gruntas);
            let settings = theirs.into_iter().fold(Value::Object(Map::new()), merge);
            if req.sandbox {
                ours = merge(ours, claude_sandbox(&req.hidden));
                if req.mode == Mode::Attended {
                    // A proxy the person named stays theirs.
                    let named = settings["sandbox"]["network"].get("httpProxyPort").is_some() || settings["sandbox"]["network"].get("socksProxyPort").is_some();
                    ours = merge(ours, claude_attended(&req.hidden, req.egress.filter(|_| !named)));
                }
            }
            let settings = merge(settings, ours);
            if let Mode::Unattended { .. } = req.mode {
                plan.args.push("--bg".into());
            }
            plan.args.extend(["--plugin-dir".into(), req.plugin_dir.display().to_string(), "--settings".into(), settings.to_string()]);
            plan.args.extend(configured);
            plan.args.extend(rest);
            if let Mode::Unattended { task } = &req.mode {
                plan.args.push(task.clone());
            }
            plan.settings = Some(settings);
        }
        Harness::Codex => {
            // After the instance's own `-c`s, so none of them picks another
            // profile; before the person's, which may name a subcommand.
            // Unattended, the engine puts the instance's own after
            // `app-server` itself, and the profile after them.
            let attended = req.mode == Mode::Attended;
            if attended {
                plan.args.extend(b.args.iter().cloned());
            }
            if req.sandbox {
                plan.args.extend(codex_profile(&req.hidden, true));
            }
            if attended {
                plan.args.extend(req.args.iter().cloned());
            }
        }
    }
    Ok(plan)
}

/// The `--settings` the person passed, in order (`--settings X` or
/// `--settings=X`; a JSON object or a file holding one), and the rest of
/// their arguments as they were.
fn take_settings(args: &[String], read: &dyn Fn(&Path) -> std::io::Result<String>) -> Result<(Vec<Value>, Vec<String>), String> {
    let (mut found, mut rest) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            rest.push(a.clone());
            rest.extend(it.by_ref().cloned());
            break;
        }
        let value = match a.strip_prefix("--settings") {
            Some("") => it.next().ok_or("--settings needs a value: a JSON object or a settings file")?.clone(),
            Some(v) if v.starts_with('=') => v[1..].to_string(),
            _ => {
                rest.push(a.clone());
                continue;
            }
        };
        let text = match value.trim_start().starts_with('{') {
            true => value.clone(),
            false => read(Path::new(&value)).map_err(|e| format!("--settings {value}: {e}"))?,
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v @ Value::Object(_)) => found.push(v),
            _ => return Err(format!("--settings {value}: not a JSON object")),
        }
    }
    Ok((found, rest))
}

/// `ours` laid over `base`: objects merged key by key, an array given the
/// entries it lacks, anything else ours.
pub fn merge(base: Value, ours: Value) -> Value {
    match (base, ours) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(old) => merge(old, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        (Value::Array(mut b), Value::Array(o)) => {
            for v in o {
                if !b.contains(&v) {
                    b.push(v);
                }
            }
            Value::Array(b)
        }
        (_, ours) => ours,
    }
}

/// Every path a launched session is kept out of, the gruntas home `home`
/// (resolved) first: what gruntas's own tools hold secret
/// (`permissions::Policy::load`) — that home and the default `~/.gruntas`,
/// each with its migration staging directory and lock, as named and as
/// they lead, and the old `~/.krowk` where it is — and the host daemon's
/// runtime directory, its socket included. Hooks and MCP servers run
/// outside the agent's sandbox, so the plugin still reaches gruntas.
pub fn hidden(home: &Path, env: &dyn Fn(&str) -> String) -> Vec<PathBuf> {
    let user = Some(env("HOME")).filter(|h| !h.trim().is_empty()).map(|h| gruntas_api::home::lexical(Path::new(&h)).join(".gruntas"));
    let old = user.as_deref().map(gruntas_api::migrate::krowk_home).filter(|k| k.exists());
    let homes = std::iter::once(home.to_path_buf()).chain(user).chain(old).flat_map(|d| gruntas_api::home::siblings(&d));
    #[cfg(unix)]
    let homes = homes.chain([crate::slots::runtime_dir(env).join(crate::daemon::home_key(home))]);
    let mut out: Vec<PathBuf> = Vec::new();
    for p in homes.flat_map(|d| [Some(d.clone()), d.canonicalize().ok()]).flatten() {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Claude Code's block that keeps a session out of `hidden` (D4, the
/// spike's four parts, all needed): the sandbox on, no session without it,
/// no command outside it, each path unreadable to commands, and a `Read`
/// deny rule for each for Claude's own file tools (`//` is Claude Code's
/// spelling of an absolute path in a rule). The network rules are left as
/// they are.
pub fn claude_sandbox(hidden: &[PathBuf]) -> Value {
    let paths: Vec<String> = hidden.iter().map(|p| p.display().to_string()).collect();
    let deny: Vec<String> = paths.iter().map(|p| format!("Read(/{p}/**)")).collect();
    json!({
        "sandbox": { "enabled": true, "failIfUnavailable": true, "allowUnsandboxedCommands": false, "filesystem": { "denyRead": paths } },
        "permissions": { "deny": deny },
    })
}

/// What an attended session's sandbox adds to `claude_sandbox`, so the
/// person's own credentials and logins work in it as they do in their own
/// `claude`: every Unix socket (ssh-agent, the keyring over D-Bus, Docker),
/// writes anywhere in their home (a CLI refreshing its token) but the
/// hidden paths, and, through `egress`, every destination without a prompt.
/// The sandbox stays on with no command outside it, so the home stays
/// hidden from what a session reads and runs; but a soft fence, not a hard
/// one: a session set on it can reach out through the sockets (systemd over
/// D-Bus) or a file it writes that runs later (a shell's rc file). It stops
/// accidents and the naive injection, not the deliberate one (the threat
/// model's sandbox rule). An unattended session keeps the hard fence.
pub fn claude_attended(hidden: &[PathBuf], egress: Option<crate::egress::Egress>) -> Value {
    let paths: Vec<String> = hidden.iter().map(|p| p.display().to_string()).collect();
    let mut network = json!({ "allowAllUnixSockets": true });
    if let Some(e) = egress {
        network["httpProxyPort"] = json!(e.http);
        network["socksProxyPort"] = json!(e.socks);
    }
    json!({ "sandbox": { "filesystem": { "allowWrite": ["~"], "denyWrite": paths }, "network": network } })
}

/// The WorktreeCreate and WorktreeRemove hooks, run by `gruntas`, the case
/// the plugin's own misses (the spike): Claude Code's worktrees come from
/// gruntas, as the plugin's hooks make them.
pub fn worktree_hooks(gruntas: &Path) -> Value {
    let hook = |event: &str| json!([{ "hooks": [{ "type": "command", "command": format!("{} hook claude {event}", crate::worktree::manage::quote(gruntas)), "timeout": 900 }] }]);
    json!({ "hooks": { "WorktreeCreate": hook("WorktreeCreate"), "WorktreeRemove": hook("WorktreeRemove") } })
}

/// The Codex permissions profile a launch runs in (D3).
pub const CODEX_PROFILE: &str = "gruntas";

/// Codex's overrides for `CODEX_PROFILE`: chosen as the default
/// permissions, reading everything, writing the project when `write`, and
/// each of `hidden` `none` — a built-in profile still reads the home (the
/// spike). Codex 0.171 has no `-P`; `default_permissions` picks the profile.
pub fn codex_profile(hidden: &[PathBuf], write: bool) -> Vec<String> {
    let project = if write { "write" } else { "read" };
    let none: String = hidden.iter().map(|p| format!(",{}=\"none\"", toml_string(&p.display().to_string()))).collect();
    vec![
        "-c".into(),
        format!("default_permissions=\"{CODEX_PROFILE}\""),
        "-c".into(),
        format!("permissions.{CODEX_PROFILE}.filesystem={{\":root\"=\"read\",\":project_roots\"=\"{project}\"{none}}}"),
    ]
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Where the gruntas plugin a launch loads is kept: `plugin` beside the
/// worktrees gruntas makes (`$XDG_DATA_HOME/gruntas/plugin`), outside the
/// gruntas home the sandbox hides and outside every account. None without
/// a user home.
pub fn plugin_dir(env: &dyn Fn(&str) -> String) -> Option<PathBuf> {
    Some(gruntas_api::home::worktrees_root(env)?.parent()?.join("plugin"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::{InstanceKind, InstancesConfig};
    use std::collections::BTreeMap;

    fn env(k: &str) -> String {
        if k == "HOME" { "/h".into() } else { String::new() }
    }

    fn registry() -> Registry {
        let mut cfg = InstancesConfig::default();
        let extra = BTreeMap::from([("ANTHROPIC_BASE_URL".to_string(), "https://router".to_string())]);
        cfg.instances.insert("claude:work".into(), InstanceKind::ClaudeCode { binary: Some("/opt/claude".into()), config_dir: Some("/h/.gruntas/accounts/claude-work".into()), env: extra, args: vec!["--verbose".into()], api_key_env: None, effort: None });
        cfg.instances.insert("codex:team".into(), InstanceKind::CodexAppServer { binary: None, codex_home: Some("/h/.gruntas/accounts/codex-team".into()), env: BTreeMap::new(), args: vec!["-c".into(), "model_provider=\"x\"".into()], api_key_env: None, effort: None });
        // The default account, configured with Claude Code's own home.
        cfg.instances.insert("claude".into(), InstanceKind::ClaudeCode { binary: None, config_dir: Some("/h/.claude".into()), env: BTreeMap::new(), args: vec![], api_key_env: None, effort: None });
        cfg.instances.insert("claude:own".into(), InstanceKind::ClaudeCode { binary: None, config_dir: None, env: BTreeMap::new(), args: vec!["--settings".into(), r#"{"sandbox":{"enabled":false},"model":"opus"}"#.into()], api_key_env: None, effort: None });
        Registry::resolve(&cfg, &env)
    }

    fn request<'a>(harness: Harness, mode: Mode, instance: &'a Resolved, args: &[&str]) -> Request<'a> {
        Request {
            harness,
            mode,
            instance,
            hidden: vec!["/h/.gruntas".into()],
            plugin_dir: "/h/.local/share/gruntas/plugin".into(),
            gruntas: "/usr/bin/gruntas".into(),
            sandbox: true,
            egress: Some(crate::egress::Egress { http: 3128, socks: 1080 }),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn no_file(p: &Path) -> std::io::Result<String> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} not found", p.display())))
    }

    fn set<'p>(p: &'p Plan, k: &str) -> Option<&'p str> {
        p.env_set.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn accounts_read_as_the_harness_names_them() {
        let c = Harness::Claude;
        assert_eq!(Account::parse(c, "").unwrap(), Account::Default);
        assert_eq!(Account::parse(c, "claude").unwrap(), Account::Default);
        assert_eq!(Account::parse(c, "work").unwrap(), Account::Named("work".into()));
        assert_eq!(Account::parse(c, "claude:work").unwrap(), Account::Named("work".into()));
        assert_eq!(Account::parse(c, "auto").unwrap(), Account::Auto);
        assert!(Account::parse(c, "codex:team").unwrap_err().contains("not a claude account"));
        assert_eq!(Account::Named("work".into()).instance(c).as_deref(), Some("claude:work"));
        assert_eq!(Account::Default.instance(Harness::Codex).as_deref(), Some("codex"));
        let r = registry();
        assert_eq!(account(&r, c, "claude:work").unwrap().name, "claude:work");
        assert_eq!(account(&r, c, "claude").unwrap().name, "claude");
        assert!(account(&r, c, "claude:nope").unwrap_err().contains("gruntas connect anthropic --method subscription --name nope"));
        assert!(account(&r, c, "codex:team").unwrap_err().contains("not a claude account"));
    }

    #[test]
    fn claude_attended_carries_the_home_the_plugin_and_one_merged_settings() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude:work").unwrap();
        let theirs = r#"{"sandbox":{"network":{"allowedDomains":["x.dev"]},"filesystem":{"denyRead":["/secret"]}},"permissions":{"deny":["Bash(rm:*)"]},"model":"opus"}"#;
        let p = plan(&request(Harness::Claude, Mode::Attended, inst, &["--model", "sonnet", "--settings", theirs, "fix it"]), &no_file).unwrap();
        assert_eq!(p.program, PathBuf::from("/opt/claude"), "not there: as configured");
        assert_eq!(set(&p, "CLAUDE_CONFIG_DIR"), Some("/h/.gruntas/accounts/claude-work"));
        assert_eq!(set(&p, "ANTHROPIC_BASE_URL"), Some("https://router"));
        assert!(p.env_remove.contains(&"ANTHROPIC_API_KEY") && !p.env_remove.contains(&"ANTHROPIC_BASE_URL"));
        let s = p.settings.clone().unwrap();
        let want = json!({
            "sandbox": {
                "network": { "allowedDomains": ["x.dev"], "allowAllUnixSockets": true, "httpProxyPort": 3128, "socksProxyPort": 1080 },
                "filesystem": { "denyRead": ["/secret", "/h/.gruntas"], "allowWrite": ["~"], "denyWrite": ["/h/.gruntas"] },
                "enabled": true, "failIfUnavailable": true, "allowUnsandboxedCommands": false,
            },
            "permissions": { "deny": ["Bash(rm:*)", "Read(//h/.gruntas/**)"] },
            "model": "opus",
        });
        for k in ["sandbox", "permissions", "model"] {
            assert_eq!(s[k], want[k], "{k}");
        }
        assert_eq!(s["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"], "/usr/bin/gruntas hook claude WorktreeCreate");
        assert_eq!(s["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"], "/usr/bin/gruntas hook claude WorktreeRemove");
        let args = vec!["--plugin-dir".to_string(), "/h/.local/share/gruntas/plugin".into(), "--settings".into(), s.to_string(), "--verbose".into(), "--model".into(), "sonnet".into(), "fix it".into()];
        assert_eq!(p.args, args, "one --settings, the person's other arguments untouched");
        assert!(p.sandboxed);
    }

    #[test]
    fn a_settings_file_is_read_and_a_bad_one_refused() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude").unwrap();
        let file = |p: &Path| if p == Path::new("mine.json") { Ok(r#"{"hooks":{"Stop":[{"hooks":[]}]}}"#.into()) } else { no_file(p) };
        let p = plan(&request(Harness::Claude, Mode::Attended, inst, &["--settings=mine.json"]), &file).unwrap();
        let s = p.settings.as_ref().unwrap();
        assert!(s["hooks"]["Stop"].is_array() && s["hooks"]["WorktreeCreate"].is_array());
        assert_eq!(p.args.iter().filter(|a| *a == "--settings").count(), 1);
        assert!(p.config_home.is_none() && set(&p, "CLAUDE_CONFIG_DIR").is_none(), "the default account keeps Claude's own home");
        assert!(!p.env_remove.contains(&"CLAUDE_CONFIG_DIR"), "an inherited one is kept: the default is what `claude` in the shell is");
        for bad in [&["--settings", "gone.json"][..], &["--settings", "[1]"], &["--settings"]] {
            assert!(plan(&request(Harness::Claude, Mode::Attended, inst, bad), &no_file).is_err(), "{bad:?}");
        }
        // After `--`, nothing is the person's --settings.
        let p = plan(&request(Harness::Claude, Mode::Attended, inst, &["--", "--settings", "x"]), &no_file).unwrap();
        assert!(p.args.ends_with(&["--".into(), "--settings".into(), "x".into()]));
    }

    /// An instance's configured `--settings` is merged too, never a second
    /// one that would replace gruntas's.
    #[test]
    fn an_instances_own_settings_cannot_replace_the_sandbox() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude:own").unwrap();
        let p = plan(&request(Harness::Claude, Mode::Attended, inst, &[]), &no_file).unwrap();
        assert_eq!(p.args.iter().filter(|a| *a == "--settings").count(), 1, "{:?}", p.args);
        let s = p.settings.unwrap();
        assert_eq!((&s["sandbox"]["enabled"], &s["model"]), (&json!(true), &json!("opus")));
    }

    #[test]
    fn no_sandbox_drops_the_sandbox_parts_and_keeps_the_hooks() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude").unwrap();
        let p = plan(&Request { sandbox: false, ..request(Harness::Claude, Mode::Attended, inst, &[]) }, &no_file).unwrap();
        let s = p.settings.unwrap();
        assert!(s.get("sandbox").is_none() && s.get("permissions").is_none(), "{s}");
        assert!(s["hooks"]["WorktreeCreate"].is_array());
        assert!(!p.sandboxed);
        let codex = account(&r, Harness::Codex, "codex").unwrap();
        let p = plan(&Request { sandbox: false, ..request(Harness::Codex, Mode::Attended, codex, &["resume"]) }, &no_file).unwrap();
        assert_eq!(p.args, ["resume"]);
        // Unattended never runs without it (D4).
        let task = Mode::Unattended { task: "t".into() };
        for (h, i) in [(Harness::Claude, inst), (Harness::Codex, codex)] {
            assert!(plan(&Request { sandbox: false, ..request(h, task.clone(), i, &[]) }, &no_file).unwrap_err().contains("--no-sandbox"));
        }
    }

    /// Attended, a proxy the person named in their settings is kept, and
    /// without one of gruntas's the rest still applies.
    #[test]
    fn attended_keeps_the_persons_proxy() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude").unwrap();
        let theirs = r#"{"sandbox":{"network":{"httpProxyPort":9000}}}"#;
        let p = plan(&request(Harness::Claude, Mode::Attended, inst, &["--settings", theirs]), &no_file).unwrap();
        let n = &p.settings.unwrap()["sandbox"]["network"];
        assert_eq!((&n["httpProxyPort"], n.get("socksProxyPort"), &n["allowAllUnixSockets"]), (&json!(9000), None, &json!(true)));
        let p = plan(&Request { egress: None, ..request(Harness::Claude, Mode::Attended, inst, &[]) }, &no_file).unwrap();
        let s = p.settings.unwrap();
        assert!(s["sandbox"]["network"].get("httpProxyPort").is_none() && s["sandbox"]["filesystem"]["allowWrite"] == json!(["~"]));
    }

    #[test]
    fn claude_unattended_starts_in_the_background_on_the_task() {
        let r = registry();
        let inst = account(&r, Harness::Claude, "claude:work").unwrap();
        let p = plan(&request(Harness::Claude, Mode::Unattended { task: "fix the build".into() }, inst, &["--model", "opus"]), &no_file).unwrap();
        assert_eq!(p.args[0], "--bg");
        assert_eq!(p.args.last().unwrap(), "fix the build");
        let s = p.settings.as_ref().unwrap();
        assert_eq!(s["sandbox"], claude_sandbox(&["/h/.gruntas".into()])["sandbox"], "unattended keeps the narrow sandbox: no sockets, no proxy, no writes outside");
        assert!(p.args.windows(2).any(|w| w == ["--model", "opus"]));
        assert_eq!(set(&p, "CLAUDE_CONFIG_DIR"), Some("/h/.gruntas/accounts/claude-work"));
    }

    #[test]
    fn codex_runs_in_a_profile_that_hides_the_home() {
        let r = registry();
        let inst = account(&r, Harness::Codex, "codex:team").unwrap();
        let p = plan(&request(Harness::Codex, Mode::Attended, inst, &["--model", "gpt-5.5"]), &no_file).unwrap();
        assert_eq!(set(&p, "CODEX_HOME"), Some("/h/.gruntas/accounts/codex-team"));
        assert!(p.env_remove.contains(&"OPENAI_API_KEY"));
        assert_eq!(p.program, PathBuf::from("codex"), "not on PATH: its name");
        let want = ["-c", "model_provider=\"x\"", "-c", "default_permissions=\"gruntas\"", "-c", r#"permissions.gruntas.filesystem={":root"="read",":project_roots"="write","/h/.gruntas"="none"}"#, "--model", "gpt-5.5"];
        assert_eq!(p.args, want, "the profile after the instance's own -c, before the person's");
        assert!(p.settings.is_none());
        // Unattended: the overrides the engine adds after `app-server`;
        // the task is the engine's first turn, not an argument.
        let p = plan(&request(Harness::Codex, Mode::Unattended { task: "t".into() }, inst, &[]), &no_file).unwrap();
        assert_eq!(p.args, want[2..6]);
        assert_eq!(toml_string("/a \"b\"\\c"), r#""/a \"b\"\\c""#);
    }

    /// The fence hides what gruntas's own tools hold secret, and the
    /// daemon's runtime directory, in both harnesses.
    #[test]
    fn the_fence_hides_the_homes_their_siblings_and_the_daemon() {
        let run = |k: &str| match k {
            "HOME" => "/h".into(),
            "XDG_RUNTIME_DIR" => "/run/user/7".into(),
            _ => String::new(),
        };
        let hidden = hidden(Path::new("/g/home"), &run);
        let has = |p: &str| hidden.contains(&PathBuf::from(p));
        assert_eq!(hidden[0], PathBuf::from("/g/home"), "the home in use first");
        assert!(has("/h/.gruntas") && hidden.len() >= 7, "{hidden:?}");
        for d in ["/g/home", "/h/.gruntas"] {
            assert!(gruntas_api::home::siblings(Path::new(d)).iter().all(|s| hidden.contains(s)), "{d}'s staging and lock: {hidden:?}");
        }
        assert!(!hidden.contains(&gruntas_api::migrate::krowk_home(Path::new("/h/.gruntas"))), "no old home where there is none");
        #[cfg(unix)]
        assert!(hidden.contains(&PathBuf::from("/run/user/7/gruntas").join(crate::daemon::home_key(Path::new("/g/home")))), "the daemon's socket directory");
        let two = [PathBuf::from("/a"), PathBuf::from("/b")];
        let s = claude_sandbox(&two);
        assert_eq!((&s["sandbox"]["filesystem"]["denyRead"], &s["permissions"]["deny"]), (&json!(["/a", "/b"]), &json!(["Read(//a/**)", "Read(//b/**)"])));
        assert_eq!(codex_profile(&two, false)[3], r#"permissions.gruntas.filesystem={":root"="read",":project_roots"="read","/a"="none","/b"="none"}"#);
    }

    #[test]
    fn the_plugin_is_kept_beside_the_worktrees() {
        assert_eq!(plugin_dir(&env), Some(PathBuf::from("/h/.local/share/gruntas/plugin")));
        let xdg = |k: &str| if k == "XDG_DATA_HOME" { "/d".into() } else { env(k) };
        assert_eq!(plugin_dir(&xdg), Some(PathBuf::from("/d/gruntas/plugin")));
    }

    /// R-RUN-5: making a plan touches no file, so an account's settings,
    /// credentials and plugins are as they were.
    #[test]
    fn no_plan_writes_into_an_account() {
        let dir = std::env::temp_dir().join(format!("gruntas-launch-{}", std::process::id()));
        let home = dir.join("accounts/claude-work");
        std::fs::create_dir_all(home.join("plugins")).unwrap();
        std::fs::write(home.join("settings.json"), "{}").unwrap();
        let before = listing(&dir);
        let mut cfg = InstancesConfig::default();
        cfg.instances.insert("claude:work".into(), InstanceKind::ClaudeCode { binary: None, config_dir: Some(home.display().to_string()), env: BTreeMap::new(), args: vec![], api_key_env: None, effort: None });
        cfg.instances.insert("codex:team".into(), InstanceKind::CodexAppServer { binary: None, codex_home: Some(home.display().to_string()), env: BTreeMap::new(), args: vec![], api_key_env: None, effort: None });
        let r = Registry::resolve(&cfg, &env);
        for (h, name) in [(Harness::Claude, "claude:work"), (Harness::Codex, "codex:team")] {
            for mode in [Mode::Attended, Mode::Unattended { task: "t".into() }] {
                let req = Request { hidden: vec![dir.clone()], ..request(h, mode, account(&r, h, name).unwrap(), &[]) };
                plan(&req, &|p| std::fs::read_to_string(p)).unwrap();
            }
        }
        assert_eq!(listing(&dir), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn listing(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push((p.clone(), Vec::new()));
                out.extend(listing(&p));
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
        out.sort();
        out
    }
}
