//! `krowk mcp login|logout`: signing in to a remote MCP server that asks for
//! OAuth, and forgetting the sign-in. The flow and the storage are
//! `krowk_harness::mcp_oauth`; this resolves the server by name from the
//! same config krowk's agent reads, and talks to the person.

use super::{prompt, providers, Ctx};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::mcp_oauth;
use serde_json::json;
use std::sync::Arc;

fn name_arg(verb: &str, args: &[String]) -> Result<String, Error> {
    match args {
        [n] => Ok(n.clone()),
        [] => Err(fail("missing_argument", format!("`krowk mcp {verb}` needs the server's name"))),
        _ => Err(fail("unexpected_argument", format!("`krowk mcp {verb}` takes one server name, and got `{}`", args.join(" ")))),
    }
}

/// The URL of the HTTP server `name` in the config krowk's agent reads.
fn server_url(ctx: &Ctx, name: &str) -> Result<String, Error> {
    let config = prompt::config_json()?;
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let store = krowk_harness::trust::Store::new(Some(providers::krowk_dir()?.join(krowk_harness::trust::FILE)), home);
    let cfg = prompt::permissions_config(ctx, &config, Arc::new(move |root: &std::path::Path| store.trusts(root)), false);
    let cwd = std::env::current_dir().map_err(|e| fail("cwd_unavailable", e.to_string()))?;
    let (servers, _) = krowk_harness::mcp::discover(&cfg, &cwd);
    let server = servers.iter().find(|s| s.name == name).ok_or_else(|| fail("unknown_server", format!("no MCP server named {name:?} in the config krowk reads (a repository's .mcp.json counts only in a trusted repository)")))?;
    server.config.url.clone().ok_or_else(|| fail("not_http", format!("{name} is a local server, which has no sign-in — only a server with a url does")))
}

pub(super) fn login(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let name = name_arg("login", args)?;
    let url = server_url(ctx, &name)?;
    let credentials = providers::credentials_path()?;
    let paste = ctx.f.no_browser || super::auth::headless(ctx);
    let mut tell = |page: &str| {
        let opened = !paste && super::auth::open_browser(page);
        let err = &mut *ctx.io.stderr;
        let _ = if paste {
            writeln!(err, "Open this page to sign in to {name}:\n{page}\nThen paste the address your browser lands on:")
        } else if opened {
            writeln!(err, "Opened your browser to sign in to {name}. Waiting…")
        } else {
            writeln!(err, "Open this page to sign in to {name}:\n{page}\nWaiting…")
        };
        let _ = err.flush();
    };
    let mut read = || -> Result<String, krowk_harness::engine::EngineError> {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|e| krowk_harness::engine::EngineError::new("stdin_unreadable", format!("could not read the pasted address: {e}")))?;
        Ok(line)
    };
    let ui = mcp_oauth::Ui { tell: &mut tell, paste: if paste { Some(&mut read) } else { None } };
    mcp_oauth::sign_in(&credentials, &name, &url, ui).map_err(|e| prompt::engine_error(&e.code, &e.message, e.status))?;
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({ "server": name, "url": url, "signed_in": true }), format!("signed in to {name}"));
    }
    let _ = writeln!(ctx.io.stdout, "signed in to {name}: its tokens are in {}", credentials.display());
    Ok(())
}

pub(super) fn logout(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let name = name_arg("logout", args)?;
    let credentials = providers::credentials_path()?;
    let had = mcp_oauth::remove(&credentials, &name).map_err(|e| prompt::engine_error(&e.code, &e.message, e.status))?;
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({ "server": name, "had_login": had }), format!("signed out of {name}"));
    }
    let _ = if had { writeln!(ctx.io.stdout, "signed out of {name}: its tokens and registration are deleted") } else { writeln!(ctx.io.stdout, "{name} was not signed in") };
    Ok(())
}
