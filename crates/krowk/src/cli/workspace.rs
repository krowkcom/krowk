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
        global_path: krowk_api::creds::shown(&config::global_path()),
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

pub(crate) fn config_unset(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let Some(key) = args.first() else {
        return Err(fail(
            "missing_argument",
            format!("pass the key: `krowk config unset workspace` — keys: {}", config::known_keys().join(", ")),
        ));
    };
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
        return config::global_path().ok_or_else(|| fail("config_unwritable", krowk_api::creds::NO_HOME));
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
