//! `config` and `workspaces`: which workspace a command lands in, and the
//! stored keys it could land in.

use super::{interactive, Ctx};
use crate::config;
use crate::output::workspace::{self as render, ConfigView, Workspaces};
use krowk_api::creds::{self, WorkspaceKey};
use krowk_api::{fail, Error};

/// Which workspace this command was pointed at, and by whom. Empty means
/// nothing asked — "use the stored default", which only the credential store
/// can resolve. A config file that cannot be read is a refusal: somebody wrote
/// it meaning to steer uploads.
pub(crate) fn resolve_workspace(ctx: &Ctx) -> Result<(String, String), Error> {
    let cfg = config::load("", ctx.io.env, &ctx.f.workspace)
        .map_err(|e| fail("bad_config", format!("{e} — fix the file or remove it; `krowk config show` names every layer")))?;
    let source = cfg.sources.get("workspace").cloned().unwrap_or_default();
    Ok((cfg.workspace, source))
}

pub(crate) fn workspaces_list(ctx: &mut Ctx) -> Result<(), Error> {
    let (ws, source) = resolve_workspace(ctx)?;
    let stored = creds::stored_workspaces();
    let mut view = Workspaces { resolved: ws.clone(), source, shadowed: !ctx.env("KROWK_TOKEN").is_empty(), ..Workspaces::default() };
    if let Some(k) = stored.iter().find(|k| k.default).filter(|_| ws.is_empty()) {
        view.resolved = k.name.clone();
        view.source = "stored default".into();
    }
    if !view.resolved.is_empty() && !view.shadowed && creds::resolve_token(ctx.io.env, &ws).is_err() {
        view.key_missing = true;
    }
    view.stored = stored;
    let rendered = render::workspace_list(&view, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn workspaces_use(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if creds::stored_workspaces().is_empty() {
        return Err(fail("not_authenticated", "no keys are stored to choose between — `krowk login` adds one"));
    }
    let name = match args.first() {
        Some(name) => name.clone(),
        None => ask_for_workspace(
            ctx,
            "Which workspace should be the default?",
            fail("no_workspace", "pass the workspace: `krowk workspaces use <name>` — `krowk workspaces` lists them"),
        )?,
    };
    let path = creds::set_default_workspace(&name).map_err(|e| fail("unknown_workspace", e))?;
    let rendered = render::default_workspace(&name, &path, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn config_show(ctx: &mut Ctx) -> Result<(), Error> {
    let cfg = config::load("", ctx.io.env, &ctx.f.workspace).map_err(|e| fail("bad_config", e))?;
    let view = ConfigView {
        workspace: cfg.workspace,
        sources: cfg.sources,
        global_path: config::global_text(),
        repo_path: config::repo_path("").map(|p| p.display().to_string()).unwrap_or_default(),
    };
    let rendered = render::config_show(&view, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn config_set(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let missing = || {
        fail(
            "missing_argument",
            format!(
                "pass the key and the value: `krowk config set workspace <name>` — keys: {}",
                config::known_keys().join(", ")
            ),
        )
    };
    #[cfg(feature = "harness")]
    if let Some(key) = args.first().filter(|k| config::GLOBAL_ONLY.contains(&k.as_str())) {
        return config_set_global(ctx, key, args.get(1).map(String::as_str));
    }
    let (key, value) = match args {
        [] => return Err(missing()),
        [key] if key != "workspace" => return Err(missing()),
        [key] => (key.clone(), ask_for_workspace(ctx, "Which workspace should this pin?", missing())?),
        [key, value, ..] => (key.clone(), value.clone()),
    };
    config::known(&key).map_err(|e| fail("unknown_config_key", e))?;
    let path = config_file(ctx)?;
    config::set(&path, &key, &value).map_err(|e| fail("config_unwritable", format!("could not write {}: {e}", path.display())))?;
    let rendered = render::config_wrote(&key, &value, &path.display().to_string(), ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// `config set defaultMode|rollover`: always the global config, as the
/// terminal UI's settings wrote it.
#[cfg(feature = "harness")]
fn config_set_global(ctx: &mut Ctx, key: &str, value: Option<&str>) -> Result<(), Error> {
    use crate::settings;
    use krowk_harness::protocol::Rollover;
    let valid = if key == "rollover" { Rollover::NAMES.join("|") } else { settings::DEFAULT_MODES.map(|m| m.name()).join("|") };
    let Some(value) = value else {
        return Err(fail("missing_argument", format!("pass the value: `krowk config set {key} <{valid}>`")));
    };
    let path = config::global_path()?;
    let write_failed = |e: String| fail("config_unwritable", format!("could not write {}: {e}", path.display()));
    let value = if key == "rollover" {
        let r = Rollover::parse(value).map_err(|e| fail("bad_config_value", e))?;
        settings::set_rollover(&path, r).map_err(write_failed)?;
        settings::rollover_name(r)
    } else {
        let m = settings::parse_default_mode(value).map_err(|e| fail("bad_config_value", e))?;
        settings::set_default_mode(&path, m).map_err(write_failed)?;
        m.name()
    };
    let rendered = render::config_wrote(key, value, &path.display().to_string(), ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// `config get <key>`: the effective value and the layer it came from.
pub(crate) fn config_get(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let Some(key) = args.first() else {
        return Err(fail("missing_argument", format!("pass the key: `krowk config get workspace` — keys: {}", config::known_keys().join(", "))));
    };
    config::known(key).map_err(|e| fail("unknown_config_key", e))?;
    let (value, source) = match key.as_str() {
        "workspace" => {
            let cfg = config::load("", ctx.io.env, &ctx.f.workspace).map_err(|e| fail("bad_config", e))?;
            let source = cfg.sources.get("workspace").cloned().unwrap_or_else(|| "none".into());
            (cfg.workspace, source)
        }
        #[cfg(feature = "harness")]
        other => effective_global(ctx, other)?,
        #[cfg(not(feature = "harness"))]
        _ => unreachable!("known() admits workspace alone"),
    };
    let rendered = render::config_got(key, &value, &source, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// The effective `defaultMode` or `rollover` for the working directory.
#[cfg(feature = "harness")]
fn effective_global(ctx: &Ctx, key: &str) -> Result<(String, String), Error> {
    use crate::settings;
    use krowk_harness::{permissions, protocol::PermissionMode, trust};
    use std::sync::Arc;
    let config = super::prompt::config_json()?;
    let raw = config.as_object().cloned().unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let store = trust::Store::new(Some(super::providers::krowk_dir()?.join(trust::FILE)), home);
    let permissions = super::prompt::permissions_config(ctx, &config, Arc::new(move |root: &std::path::Path| store.trusts(root)), false);
    let loaded = permissions::settings::load(&permissions, &cwd).map_err(super::prompt::bad_settings)?;
    if key == "rollover" {
        let global = settings::global_rollover(&raw).map(|v| krowk_harness::protocol::Rollover::parse(&v)).transpose().map_err(|e| fail("bad_config", format!("{}: {e}", config::global_text())))?;
        let (r, source) = settings::effective_rollover(loaded.rollover, global);
        return Ok((settings::rollover_name(r).into(), source.into()));
    }
    let saved = settings::default_mode(&raw);
    let effective = loaded.default_mode.unwrap_or_default();
    // The value config.json names is the source unless a file read after it
    // (a trusted repository's, Claude Code's) set another.
    let source = match saved.as_deref().and_then(PermissionMode::parse) {
        _ if saved.is_none() && loaded.default_mode.is_none() => "default",
        Some(m) if m == effective => "global",
        _ => "repo or Claude Code settings",
    };
    Ok((effective.name().into(), source.into()))
}

pub(crate) fn config_unset(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let Some(key) = args.first() else {
        return Err(fail(
            "missing_argument",
            format!("pass the key: `krowk config unset workspace` — keys: {}", config::known_keys().join(", ")),
        ));
    };
    if config::GLOBAL_ONLY.contains(&key.as_str()) {
        return Err(fail("bad_config_key", format!("{key} cannot be removed — set it to a value instead: `krowk config set {key} <value>`")));
    }
    config::known(key).map_err(|e| fail("unknown_config_key", e))?;
    let path = config_file(ctx)?;
    config::unset(&path, key).map_err(|e| fail("config_unwritable", format!("could not write {}: {e}", path.display())))?;
    let rendered = render::config_wrote(key, "", &path.display().to_string(), ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// The file a write goes to: the machine's under --global, else the
/// repository's, and a refusal outside one.
fn config_file(ctx: &Ctx) -> Result<std::path::PathBuf, Error> {
    if ctx.f.global {
        return config::global_path();
    }
    config::repo_path("").ok_or_else(|| {
        fail(
            "not_in_a_repository",
            "the repo config lives at <git-root>/.krowk/config.json and there is no git root here — run it inside the \
             repository, or pass --global for the machine-wide file",
        )
    })
}

/// A person at a terminal picks from the stored keys; anyone else gets
/// `otherwise`, since a question with nobody to answer it is a hang.
fn ask_for_workspace(ctx: &Ctx, title: &str, otherwise: Error) -> Result<String, Error> {
    if !interactive(ctx) {
        return Err(otherwise);
    }
    let stored = creds::stored_workspaces();
    if stored.is_empty() {
        return Err(fail("not_authenticated", "no keys are stored to pick from — `krowk login` adds one"));
    }
    pick_workspace(title, &stored)
}

fn pick_workspace(title: &str, stored: &[WorkspaceKey]) -> Result<String, Error> {
    let labels: Vec<String> = stored
        .iter()
        .map(|k| {
            let mut label = if k.workspace_name.is_empty() { k.name.clone() } else { format!("{}  —  {}", k.workspace_name, k.name) };
            if k.default {
                label += "  (default)";
            }
            label
        })
        .collect();
    let picked = inquire::Select::new(title, labels.clone())
        .prompt()
        .map_err(|_| fail("selection_cancelled", "nothing was selected and nothing was changed"))?;
    let at = labels.iter().position(|l| *l == picked).expect("picked from the list");
    Ok(stored[at].name.clone())
}
