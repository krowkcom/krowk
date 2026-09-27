//! `krowk connect` and `krowk disconnect`: a model source, by vendor and
//! method — a Claude subscription through Claude Code's own login
//! (`claude:work`, R-INST-2), a ChatGPT subscription through Codex's
//! (`codex:team`), SuperGrok through krowk's own OAuth, or an API key read
//! from an environment variable — and `krowk providers add|list|remove`,
//! the same instances by backend kind, one level down. The sign-in itself
//! is `krowk_harness::connect`, which the TUI shares; this file is the
//! terminal it asks and tells through, and the words of each result.
//!
//! A definition names the variable its key is read from and never holds the
//! key, so config.json stays something that can sync between hosts; a key
//! pasted, piped (`--key-stdin`) or referenced (`--key-ref`) is stored in
//! the provider credentials file instead (`krowk_harness::keys`). A
//! Claude Code login is Claude Code's: krowk asks `claude auth status`
//! whether there is one and never reads it (R-BACK-2); a Codex login is
//! Codex's, asked of `codex app-server`'s `account/read`, or `codex login
//! status` (R-BACK-3).

use super::{auth, Ctx};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::connect::{self, Answer, AuthInteraction, Connected, MakeDefault, Method, Notice, Options, Prompt, ProviderAuth, Request, SignedOut};
use krowk_harness::engine::EngineError;
use krowk_harness::instances::{kind_label, Auth};
use krowk_harness::keys::KeyRef;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

/// krowk's home, for the harness: without one there is none, and the
/// harness runs nothing rather than read keys, trust or commands from
/// anywhere else.
pub(super) fn krowk_dir() -> Result<PathBuf, Error> {
    krowk_api::home::get()
}

/// krowk's one credentials file, in its home.
pub(super) fn credentials_path() -> Result<PathBuf, Error> {
    krowk_api::creds::credentials_path()
}

/// config.json, for the harness.
pub(super) fn config_path() -> Result<PathBuf, Error> {
    crate::config::global_path()
}

const PROVIDERS: &[&str] = &["anthropic", "openai", "xai", "openrouter", "openai-compatible", "supergrok", "claude", "codex"];

fn engine(e: EngineError) -> Error {
    fail(&e.code, e.message)
}

/// Text from elsewhere — an authorization server's URL and code — reaches
/// the terminal without anything that could move the cursor or repaint it.
fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// The person at this terminal, as `krowk_harness::connect` asks and tells
/// them: stderr, so stdout stays the one answer a script parses; a prompt
/// only with a person there, else the flag that answers it.
struct Terminal<'a> {
    stderr: &'a mut dyn Write,
    interactive: bool,
    open: bool,
    colour: bool,
}

impl AuthInteraction for Terminal<'_> {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn prompt(&mut self, prompt: Prompt<'_>) -> Result<Answer, EngineError> {
        let (Prompt::Text { message, flag } | Prompt::Secret { message, flag } | Prompt::Select { message, flag, .. }) = &prompt;
        if !self.interactive {
            return Err(EngineError::new("bad_argument", format!("nobody is at a terminal to ask “{message}” — pass {flag}")));
        }
        let cancelled = |_| EngineError::new("selection_cancelled", "nothing was chosen and nothing was changed");
        match prompt {
            Prompt::Text { message, .. } => inquire::Text::new(message).prompt().map(Answer::Text).map_err(cancelled),
            Prompt::Secret { message, .. } => inquire::Password::new(message).without_confirmation().prompt().map(Answer::Text).map_err(cancelled),
            Prompt::Select { message, options, .. } => inquire::Select::new(message, options.to_vec()).raw_prompt().map(|o| Answer::Choice(o.index)).map_err(cancelled),
        }
    }

    fn notify(&mut self, notice: Notice<'_>) {
        let _ = match notice {
            // krowk's own words around a vendor's, dimmed so the vendor's
            // prompts and the result stand out.
            Notice::Info(s) | Notice::Progress(s) => writeln!(self.stderr, "{}", crate::output::paint(self.colour, crate::output::DIM, s)),
            Notice::AuthUrl { url, message } => {
                let _ = writeln!(self.stderr, "{message}\n  {}", plain(url));
                if self.open && auth::open_browser(url) {
                    let _ = writeln!(self.stderr, "(opened in your browser — waiting for it to finish)");
                }
                Ok(())
            }
            Notice::DeviceCode { url, code, message } => writeln!(self.stderr, "Open {}\nand enter the code {} {message}. Waiting…", plain(url), plain(code)),
        };
        let _ = self.stderr.flush();
    }
}

fn options(ctx: &Ctx) -> Result<Options, Error> {
    let o = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    let key = match (ctx.f.key_stdin, o(&ctx.f.key_ref)) {
        (true, Some(_)) => return Err(fail("bad_flag", "--key-stdin and --key-ref each give the key — pass one")),
        (true, None) if ctx.io.stdin_tty => return Err(fail("bad_flag", "--key-stdin reads a key piped in — at a terminal leave it out and paste the key at the prompt, which does not echo it")),
        (true, None) => {
            use std::io::Read;
            let mut raw = String::new();
            std::io::stdin().take(64 * 1024 + 1).read_to_string(&mut raw).map_err(|e| fail("bad_flag", format!("--key-stdin: {e}")))?;
            if raw.len() > 64 * 1024 {
                return Err(fail("bad_flag", "--key-stdin: more than 64 KiB was piped in, which is no key"));
            }
            // Piped in, it is the key itself, whatever it starts with.
            Some(KeyRef::literal(&raw).map_err(|e| format!("--key-stdin: {e}")))
        }
        (false, Some(r)) => Some(match KeyRef::parse(&r) {
            Ok(KeyRef::Literal(_)) => Err("--key-ref takes '$VAR' or '!command' — a key itself goes in with --key-stdin, or at the prompt".to_string()),
            k => k,
        }),
        (false, None) => None,
    };
    let key = key.transpose().map_err(|e| fail("bad_flag", e))?;
    Ok(Options { name: o(&ctx.f.name), api_key_env: o(&ctx.f.api_key_env), base_url: o(&ctx.f.base_url), client_id: o(&ctx.f.client_id), binary: o(&ctx.f.binary), config_dir: o(&ctx.f.config_dir), key })
}

/// The shared sign-in, over this invocation's config and environment, and
/// the terminal it asks at — `asks` false for a command that never asks.
fn parts<'a>(ctx: &'a mut Ctx, asks: bool) -> Result<(ProviderAuth<'a>, Terminal<'a>), Error> {
    let interactive = asks && super::interactive(ctx) && ctx.io.stdin_tty;
    let open = !ctx.f.no_browser && !auth::headless(ctx);
    let pa = ProviderAuth { config: config_path()?, credentials: credentials_path()?, env: ctx.io.env };
    // Its notices go to stderr, so stderr's terminal decides their colour.
    let colour = ctx.colour && ctx.io.err_tty;
    Ok((pa, Terminal { stderr: &mut *ctx.io.stderr, interactive, open, colour }))
}

/// `krowk connect [vendor|instance]`: the vendor, its method and the
/// account given or — with a person at the terminal — picked, then signed
/// in; again on an instance that exists renews its login.
pub(super) fn connect(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(fail("unexpected_argument", format!("`krowk connect` takes one vendor or instance, and got `{}`", args.join(" "))));
    }
    let method = match ctx.f.method.trim() {
        "" => None,
        m => Some(Method::parse(m).ok_or_else(|| fail("bad_flag", format!("--method {m:?} is not a way in — subscription, device or api-key")))?),
    };
    let opts = options(ctx)?;
    let make_default = ctx.f.default;
    let done = {
        let (pa, mut ui) = parts(ctx, true)?;
        let mut req = pa.request(args.first().map(String::as_str), method, opts, &mut ui).map_err(engine)?;
        if make_default {
            req.default = MakeDefault::Always;
        }
        pa.connect(&req, &mut ui).map_err(engine)?
    };
    report(ctx, &done, if done.renewed { "renewed" } else { "connected" })
}

pub(super) fn add(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let provider = args.first().map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    let way = PROVIDERS.contains(&provider.as_str()).then(|| connect::by_provider(&provider, ctx.f.device)).flatten();
    let Some(way) = way else {
        let said = if provider.is_empty() { "no provider".to_string() } else { format!("{provider:?}") };
        return Err(fail("bad_argument", format!("{said} is not a provider krowk's engine speaks — one of {}", PROVIDERS.join(", "))));
    };
    // `add` asks nothing: what it makes is on its command line.
    if provider == "openai-compatible" && ctx.f.name.trim().is_empty() {
        return Err(fail("bad_flag", "openai-compatible needs --name, e.g. `krowk providers add openai-compatible --name local --base-url http://127.0.0.1:11434/v1`"));
    }
    if provider == "openai-compatible" && ctx.f.base_url.trim().is_empty() {
        return Err(fail("bad_flag", "openai-compatible needs --base-url, e.g. http://127.0.0.1:11434/v1"));
    }
    let opts = options(ctx)?;
    let done = {
        let (pa, mut ui) = parts(ctx, false)?;
        pa.connect(&Request { method: way, instance: None, options: opts, default: MakeDefault::Never }, &mut ui).map_err(engine)?
    };
    report(ctx, &done, "added")
}

/// What a connection made, the same words for `connect` and `add`.
fn report(ctx: &mut Ctx, done: &Connected, verb: &str) -> Result<(), Error> {
    let path = config_path()?;
    let (instance, kind, r) = (&done.instance, &done.definition, &done.resolved);
    let def = serde_json::to_value(kind).expect("a definition serializes");
    let key_env = (r.auth == Auth::ApiKey || !r.api_key_env.is_empty()).then(|| r.api_key_env.clone());
    let unset = key_env.is_some() && r.api_key.is_empty();
    let codex = kind.tag() == "codex-app-server";
    if ctx.format != Format::Human {
        let mut report = json!({ "instance": instance, "kind": kind.tag(), "config": path, "definition": def });
        if verb != "added" {
            report["renewed"] = json!(done.renewed);
        }
        if let Some(k) = &key_env {
            report["api_key_env"] = json!(k);
        }
        if let Some(s) = r.stored.source() {
            report["key_source"] = json!(s);
            report["credentials"] = json!(credentials_path()?.display().to_string());
        }
        if done.oauth {
            report["signed_in"] = json!(true);
            report["credentials"] = json!(credentials_path()?.display().to_string());
        }
        if verb != "added" {
            report["default_model"] = json!(done.default_model);
        }
        if let Some(v) = &done.vendor {
            report["signed_in"] = json!(v.logged_in);
            report["login"] = json!(v.describe);
            if codex {
                report["shared"] = json!(v.shared);
            }
        }
        let summary = format!("{verb} {instance}");
        return super::sessions::emit_data(ctx, report, summary);
    }
    // For a person: what happened, in one line; what it is, dimmed; and the
    // command to try it. Paths and binaries are `--json`'s and `krowk
    // status`'s — a power user asks for them, nobody needs them to go on.
    let colour = ctx.colour;
    let dim = |s: &str| crate::output::paint(colour, crate::output::DIM, s);
    let mut lines = vec![format!("{} {} {instance}", crate::output::paint(colour, crate::output::GREEN, "✓"), capitalised(verb))];
    let mut facts: Vec<String> = vec![krowk_harness::instances::kind_label(kind.tag()).to_string()];
    let mut notes: Vec<String> = Vec::new();
    match &key_env {
        // A stored key, and where: never the key.
        _ if r.stored.source().is_some() => facts.push(format!("key {} in krowk's credentials file (0600)", r.stored.source().unwrap_or_default())),
        Some(k) if unset => notes.push(format!("${k} is not set here — export it before running a prompt")),
        Some(k) => facts.push(format!("key from ${k}")),
        None if done.oauth => facts.push("signed in".into()),
        None => {}
    }
    let mut another = None;
    if let Some(v) = &done.vendor {
        // The vendor's own account says it better than the kind does — when
        // there is one: a keyed router runs on its key, not a login.
        if v.logged_in {
            facts[0] = plain(&v.describe);
        }
        if codex && !v.shared.is_empty() {
            facts.push(format!("shares your Codex {}", v.shared.join(", ")));
        }
        if verb != "added" && v.logged_in && !v.ran && r.api_key_env.is_empty() {
            let (vendor, method) = krowk_harness::instances::kind_connect(kind.tag()).unwrap_or_default();
            notes.push("it was signed in already".into());
            another = Some(format!("krowk connect {vendor} --method {method} --name <new>"));
        }
    }
    if done.default_model.is_some() {
        facts.push("your default model now".into());
    }
    lines.push(dim(&format!("  {}", facts.join(" · "))));
    lines.extend(notes.iter().map(|n| dim(&format!("  ! {n}"))));
    let try_it = match &done.default_model {
        Some(_) => "krowk".to_string(),
        None => format!("krowk --model {instance}/{}", connect::default_model(kind.tag()).unwrap_or("<model>")),
    };
    lines.push(crate::output::crumb_line("try it", &try_it, colour));
    if let Some(cmd) = another {
        lines.push(crate::output::crumb_line("another account", &cmd, colour));
    }
    let _ = writeln!(ctx.io.stdout, "{}", lines.join("\n"));
    Ok(())
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `krowk disconnect [instance] [--remove]`: signs an instance out the way
/// its kind is signed in, and keeps its definition unless `--remove`.
pub(super) fn disconnect(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(fail("unexpected_argument", format!("`krowk disconnect` takes one instance, and got `{}`", args.join(" "))));
    }
    let (remove, own) = (ctx.f.remove, ctx.f.sign_out_vendor);
    let done = {
        let (pa, mut ui) = parts(ctx, true)?;
        let target = pa.disconnect_target(args.first().map(String::as_str), &mut ui).map_err(engine)?;
        pa.disconnect(&target, remove, own, &mut ui).map_err(engine)?
    };
    let instance = &done.instance;
    let config = config_path()?;
    if ctx.format != Format::Human {
        let mut report = json!({ "instance": instance, "kind": done.kind, "removed_definition": done.removed_definition, "cleared_default": done.cleared_default });
        match &done.signed_out {
            SignedOut::Tokens { had } => {
                report["signed_out"] = json!("tokens");
                report["had_login"] = json!(had);
            }
            SignedOut::Vendor { command, home } => {
                report["signed_out"] = json!("vendor");
                report["command"] = json!(command);
                report["config_dir"] = json!(home.as_ref().map(|h| h.display().to_string()));
            }
            SignedOut::Key { var } => {
                report["signed_out"] = json!("key");
                report["api_key_env"] = json!(var);
            }
            SignedOut::StoredKey { var, set } => {
                report["signed_out"] = json!("stored_key");
                report["api_key_env"] = json!(var);
                report["api_key_env_set"] = json!(set);
            }
            SignedOut::Keyless => report["signed_out"] = json!("nothing"),
        }
        return super::sessions::emit_data(ctx, report, format!("disconnected {instance}"));
    }
    let out = &mut *ctx.io.stdout;
    let _ = match &done.signed_out {
        SignedOut::Tokens { had: true } => writeln!(out, "disconnected {instance}: its tokens are deleted from {}", credentials_path()?.display()),
        SignedOut::Tokens { had: false } => writeln!(out, "{instance} had no login to delete"),
        SignedOut::Vendor { command, home } => {
            let at = home.as_ref().map(|h| format!(" in {}", h.display())).unwrap_or_default();
            writeln!(out, "disconnected {instance}: `{command}` signed it out{at}")
        }
        SignedOut::Key { var } => writeln!(out, "{instance} reads its key from ${var}, the environment's and not krowk's to delete — unset it, and take it out of your shell's startup files, to sign it out"),
        SignedOut::StoredKey { var, set } => {
            let _ = writeln!(out, "disconnected {instance}: its stored key is deleted from {}", credentials_path()?.display());
            match var {
                Some(v) if *set => writeln!(out, "${v} is set too, and is what {instance} reads now — unset it, and take it out of your shell's startup files, to sign it out"),
                Some(v) => writeln!(out, "it reads ${v} from now on, which is not set here"),
                None => Ok(()),
            }
        }
        SignedOut::Keyless => writeln!(out, "{instance} takes no key — there is nothing to sign out of"),
    };
    if done.removed_definition {
        let _ = writeln!(out, "its definition is removed from {}", config.display());
    }
    if let Some(m) = &done.cleared_default {
        let _ = writeln!(out, "the default model was {m}, so there is none now — `krowk connect <vendor> --method <method> --default` makes a connection the default");
    }
    Ok(())
}

/// Every instance, where it runs, and whether it is ready — the readiness
/// check `krowk status` prints, with the place each one runs beside it.
pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    let (cfg, reg, reports) = super::status::reports(ctx)?;
    let rows: Vec<Value> = reports
        .iter()
        .map(|rep| {
            let r = &reg.instances[&rep.instance];
            let mut row = rep.json();
            row["provider"] = json!(r.provider);
            row["wire_api"] = json!(r.wire_api.name());
            row["base_url"] = json!(r.base_url);
            row["configured"] = json!(cfg.instances.contains_key(&r.name));
            // A backend has a binary and a config directory where an API
            // has a base URL.
            if let Some(b) = &r.backend {
                row["binary"] = json!(b.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| b.binary.clone()));
                row["config_dir"] = json!(b.home.as_ref().map(|p| p.display().to_string()));
            }
            row
        })
        .collect();
    if ctx.format != Format::Human {
        let summary = format!("{} instances", rows.len());
        return super::sessions::emit_data(ctx, json!({ "instances": rows }), summary);
    }
    let width = reports.iter().map(|r| r.instance.len()).max().unwrap_or(0);
    let out = &mut *ctx.io.stdout;
    for (rep, row) in reports.iter().zip(&rows) {
        let s = |k: &str| row[k].as_str().unwrap_or_default().to_string();
        let place = if row.get("binary").is_some() { s("binary") } else { s("base_url") };
        let _ = writeln!(out, "{:<width$}  {:<20}  {:<13}  {place}  ({})", rep.instance, kind_label(rep.kind), rep.readiness.label(), rep.source);
    }
    Ok(())
}

pub(super) fn remove(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let Some(instance) = args.first().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return Err(fail("bad_argument", "name the instance: `krowk providers remove openai:work` — `krowk providers list` shows them"));
    };
    let gone = {
        let (pa, _) = parts(ctx, false)?;
        pa.remove(&instance).map_err(engine)?
    };
    // A backend account's directory holds the vendor's own login, which is
    // the vendor's to sign out of: krowk leaves it as it is.
    let kept = gone.kept.map(|(dir, codex)| {
        let how = if codex { format!("Codex's login in it — `krowk disconnect {instance}` first, or `CODEX_HOME={dir} codex logout`, signs it out") } else { format!("Claude Code's login in it — `krowk disconnect {instance}` first, or `CLAUDE_CONFIG_DIR={dir} claude auth logout`, signs it out") };
        (dir, how)
    });
    if ctx.format != Format::Human {
        let summary = format!("removed {instance}");
        let mut report = json!({ "instance": instance, "removed_definition": gone.definition, "removed_login": gone.login });
        if let Some((dir, _)) = &kept {
            report["config_dir_kept"] = json!(dir);
        }
        return super::sessions::emit_data(ctx, report, summary);
    }
    let what = match (gone.definition, gone.login) {
        (true, true) => "its definition and its login",
        (true, false) => "its definition",
        _ => "its login",
    };
    let _ = writeln!(ctx.io.stdout, "removed {instance}: {what}");
    if let Some((dir, login)) = &kept {
        let _ = writeln!(ctx.io.stdout, "its config directory {dir} is kept, with {login}");
    }
    Ok(())
}
