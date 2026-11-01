//! `krowk mcp add|remove|list`: the MCP servers krowk's own agent loads from
//! its config.json (`mcpServers`, the shape Claude Code uses), user scope.
//! A server from a repository's `.mcp.json` or Claude Code's files is not
//! krowk's to edit: `list` shows it, `remove` says which file to edit.
//! `list` reads configuration and stored sign-ins only; it connects to
//! nothing.

use super::{prompt, providers, Ctx};
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::mcp;
use krowk_harness::trust;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const RULE: &str = "a name may hold only letters, digits, - and _, with no __ and no _ at either end, so mcp__server__tool reads one way in a rule";

fn permissions(ctx: &Ctx, config: &Value) -> krowk_harness::permissions::Config {
    let home = Some(ctx.env("HOME")).filter(|h| !h.trim().is_empty()).map(std::path::PathBuf::from);
    let store = trust::Store::new(providers::krowk_dir().ok().map(|d| d.join(trust::FILE)), home);
    prompt::permissions_config(ctx, config, Arc::new(move |root: &std::path::Path| store.trusts(root)), false)
}

/// `krowk mcp add <name> <url>` or `krowk mcp add <name> [--env K=V] -- <command> [args…]`.
pub(super) fn add(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    // Positionals typed before a `--` are the name (and a URL); after it,
    // the server's command.
    let before = ctx.f.dashes.map_or(args.len(), |n| n.saturating_sub(2).min(args.len()));
    let (head, command) = args.split_at(before);
    let dashed = ctx.f.dashes.is_some();
    let name = head.first().map(|s| s.trim()).unwrap_or_default().to_string();
    if name.is_empty() {
        return Err(fail("bad_argument", "name the server: `krowk mcp add mobbin https://api.mobbin.com/mcp`, or `krowk mcp add files -- npx -y some-server`"));
    }
    if !mcp::good_name(&name) {
        return Err(fail("bad_argument", format!("{name:?} is not a server name: {RULE}")));
    }
    let config = if dashed {
        if head.len() != 1 {
            return Err(fail("bad_argument", "a local server takes only its name before `--`: `krowk mcp add files -- npx -y some-server`"));
        }
        let Some((program, rest)) = command.split_first() else {
            return Err(fail("bad_argument", "name the command after `--`: `krowk mcp add files -- npx -y some-server`"));
        };
        if !ctx.f.header.is_empty() {
            return Err(fail("bad_flag", "`--header` is for an HTTP server; a local one takes `--env`"));
        }
        let mut env = Map::new();
        for e in &ctx.f.env {
            match e.split_once('=') {
                Some((k, v)) if !k.trim().is_empty() => env.insert(k.trim().to_string(), json!(v)),
                _ => return Err(fail("bad_flag", "`--env` takes K=V, e.g. --env API_KEY=abc")),
            };
        }
        let mut c = json!({ "command": program, "args": rest });
        if !env.is_empty() {
            c["env"] = Value::Object(env);
        }
        c
    } else {
        let url = head.get(1).map(|s| s.trim()).unwrap_or_default();
        if head.len() != 2 || !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(fail("bad_argument", "give an http(s) URL for a remote server, or `--` and a command for a local one: `krowk mcp add files -- npx -y some-server`"));
        }
        if !ctx.f.env.is_empty() {
            return Err(fail("bad_flag", "`--env` is for a local server (`-- <command>`); an HTTP one takes `--header`"));
        }
        let mut headers = Map::new();
        for h in &ctx.f.header {
            match h.split_once(':') {
                Some((k, v)) if !k.trim().is_empty() => headers.insert(k.trim().to_string(), json!(v.trim())),
                _ => return Err(fail("bad_flag", "`--header` takes \"Name: value\", e.g. --header \"Authorization: Bearer ${TOKEN}\" — ${VAR} reads the variable when the server starts, so the token stays out of config.json")),
            };
        }
        let mut c = json!({ "type": "http", "url": url });
        if !headers.is_empty() {
            c["headers"] = Value::Object(headers);
        }
        c
    };
    let transport = if dashed { "stdio" } else { "http" };
    let path = providers::config_path()?;
    let current = prompt::config_json()?;
    let servers = match current.get("mcpServers") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return Err(fail("bad_config", format!("{}: \"mcpServers\" must be an object", path.display()))),
    };
    let replaced = servers.is_some_and(|m| m.contains_key(&name));
    if replaced && !ctx.f.force {
        return Err(fail("already_exists", format!("{name} is already in {} — `--force` replaces it, `krowk mcp remove {name}` removes it", path.display())));
    }
    crate::config::edit(&path, |m| {
        let s = m.entry("mcpServers").or_insert_with(|| json!({}));
        if !s.is_object() {
            *s = json!({});
        }
        s.as_object_mut().expect("an object").insert(name.clone(), config);
    })
    .map_err(|e| fail("bad_config", e))?;
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({ "name": name, "transport": transport, "config": path, "replaced": replaced }), format!("added {name}"));
    }
    let _ = writeln!(ctx.io.stdout, "{} {name} ({transport}) in {}", if replaced { "replaced" } else { "added" }, path.display());
    Ok(())
}

/// `krowk mcp remove <name>`.
pub(super) fn remove(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let Some(name) = args.first().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return Err(fail("bad_argument", "name the server: `krowk mcp remove mobbin` — `krowk mcp list` shows them"));
    };
    let path = providers::config_path()?;
    let config = prompt::config_json()?;
    if !config.get("mcpServers").and_then(|m| m.as_object()).is_some_and(|m| m.contains_key(&name)) {
        // Not krowk's to remove: say where it is.
        let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
        let perms = permissions(ctx, &config);
        let (servers, _) = mcp::discover(&perms, &cwd);
        let elsewhere = servers.iter().find(|s| s.name == name).map(|s| s.source.clone()).or_else(|| mcp::untrusted_project(&perms, &cwd).iter().any(|(n, _)| *n == name).then(|| ".mcp.json".to_string()));
        return Err(match elsewhere {
            Some(source) => {
                let file = if source == ".mcp.json" { trust::root(&cwd).join(".mcp.json").display().to_string() } else { source };
                fail("not_krowks", format!("{name} is not in krowk's config.json; it comes from {file}, which krowk does not edit — remove it there"))
            }
            None => fail("not_found", format!("no MCP server named {name} — `krowk mcp list` shows them")),
        });
    }
    crate::config::edit(&path, |m| {
        if let Some(s) = m.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
            s.remove(&name);
        }
    })
    .map_err(|e| fail("bad_config", e))?;
    // Its stored sign-in: the `mcp` section of the credentials file, which
    // may not be there at all.
    let creds = providers::credentials_path()?;
    let signed_out = creds.exists()
        && krowk_api::creds::modify::<Map<String, Value>, _>(&creds, |m| {
            let gone = m.get_mut("mcp").and_then(|s| s.as_object_mut()).is_some_and(|s| s.remove(&name).is_some());
            Ok((gone, gone))
        })
        .map_err(|e| fail("bad_credentials", format!("{name} was removed from {}, but its sign-in could not be: {e}", path.display())))?;
    if ctx.format != Format::Human {
        return super::sessions::emit_data(ctx, json!({ "name": name, "config": path, "removed_sign_in": signed_out }), format!("removed {name}"));
    }
    let _ = writeln!(ctx.io.stdout, "removed {name} from {}{}", path.display(), if signed_out { " and its stored sign-in" } else { "" });
    Ok(())
}

/// `krowk mcp list`: configuration and stored sign-ins only.
pub(super) fn list(ctx: &mut Ctx) -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(|e| fail("no_directory", format!("the working directory cannot be read: {e}")))?;
    let config = prompt::config_json()?;
    let perms = permissions(ctx, &config);
    let (servers, skipped) = mcp::discover(&perms, &cwd);
    let stored: Map<String, Value> = providers::credentials_path()
        .ok()
        .and_then(|p| krowk_api::creds::read::<Map<String, Value>>(&p).ok())
        .and_then(|m| m.get("mcp").and_then(|s| s.as_object().cloned()))
        .unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    for s in &servers {
        // Only a remote server has a sign-in; one stored for another URL
        // is no sign-in for this one.
        let sign_in = match (s.config.transport(), stored.get(&s.name)) {
            ("http", Some(e)) if e.get("url").and_then(|u| u.as_str()) == s.config.url.as_deref() => "signed in",
            ("http", Some(_)) => "not signed in",
            _ => "",
        };
        rows.push(json!({ "name": s.name, "source": s.source, "transport": s.config.transport(), "sign_in": sign_in, "untrusted": false }));
    }
    for (name, c) in mcp::untrusted_project(&perms, &cwd) {
        if !servers.iter().any(|s| s.name == name) {
            rows.push(json!({ "name": name, "source": ".mcp.json", "transport": c.transport(), "sign_in": "", "untrusted": true }));
        }
    }
    if ctx.format != Format::Human {
        let summary = format!("{} MCP servers", rows.len());
        return super::sessions::emit_data(ctx, json!({ "servers": rows, "notices": skipped }), summary);
    }
    let text = |r: &Value, k: &str| r[k].as_str().unwrap_or_default().to_string();
    let width = |k: &str| rows.iter().map(|r| text(r, k).chars().count()).max().unwrap_or(0);
    let (wn, ws, wt) = (width("name"), width("source"), width("transport"));
    let out = &mut *ctx.io.stdout;
    if rows.is_empty() {
        let _ = writeln!(out, "no MCP servers — `krowk mcp add <name> <url>` adds one");
    }
    for r in &rows {
        let state = if r["untrusted"] == json!(true) { "untrusted".to_string() } else { text(r, "sign_in") };
        let _ = writeln!(out, "{:<wn$}  {:<ws$}  {:<wt$}  {state}", text(r, "name"), text(r, "source"), text(r, "transport"));
    }
    for n in skipped {
        let _ = writeln!(out, "{n}");
    }
    Ok(())
}
