//! `krowk doctor`: describes a broken setup rather than being stopped by one,
//! so the workspace is resolved by hand and every check reports what it found.

use super::workspace::resolve_workspace;
use super::{Ctx, VERSION};
use crate::output::Format;
use krowk_api::creds::{self, TOKEN_SOURCE_NONE};
use krowk_api::Client;
use serde_json::{json, Map, Value};


pub(crate) fn doctor(ctx: &mut Ctx) -> Result<(), krowk_api::Error> {
    let resolved = resolve_workspace(ctx);
    let ws = resolved.as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
    let client = Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), &creds::read_token(ctx.io.env, &ws));

    let mut report = Map::new();
    report.insert("version".into(), json!(VERSION));
    report.insert("runtime".into(), json!(format!("rust {}/{}", std::env::consts::OS, std::env::consts::ARCH)));
    report.insert("api".into(), json!(client.base_url));
    report.insert("registry".into(), json!(registry_mode(ctx, &client)));
    report.insert("api_status".into(), json!(probe(&client)));
    report.insert("authenticated".into(), json!(client.authenticated()));
    report.insert("token_source".into(), json!(creds::token_source(ctx.io.env, &ws)));
    report.insert("key".into(), json!(key_summary(&client)));
    report.insert("workspace".into(), json!(workspace_summary(ctx, &resolved)));
    // Runs are where metadata goes, and they need a key.
    report.insert("runs_available".into(), json!(client.authenticated()));
    report.insert("credentials".into(), json!(creds::credentials_path().display().to_string()));
    report.insert("config".into(), json!(config_summary()));
    report.insert("store".into(), store_check(ctx));
    report.insert("pricing".into(), pricing_check(ctx));
    report.insert("providers".into(), providers_check(ctx));
    report.insert("context".into(), serde_json::to_value(crate::runctx::detect(ctx.io.env)).expect("context serializes"));

    if ctx.format != Format::Human {
        return ctx.emit(&crate::output::encode(&report));
    }
    let keys = ["version", "runtime", "api", "registry", "api_status", "authenticated", "token_source", "key", "workspace", "runs_available", "credentials", "config"];
    for k in keys {
        let v = match &report[k] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let _ = writeln!(ctx.io.stdout, "{k:<15} {v}");
    }
    for k in ["store", "pricing", "providers", "context"] {
        let _ = writeln!(ctx.io.stdout, "{k:<15} {}", report[k]);
    }
    Ok(())
}

#[cfg(feature = "sessions")]
fn store_check(ctx: &Ctx) -> Value {
    serde_json::to_value(krowk_store::check(ctx.io.env)).expect("check serializes")
}

/// The agent build carries no session store, and says so rather than
/// reporting on a file it cannot open.
#[cfg(not(feature = "sessions"))]
fn store_check(_: &Ctx) -> Value {
    json!({ "name": "store", "status": "skip", "message": "this build carries no session store" })
}

/// How old the prices are, read from the cache's sidecar — never fetched:
/// doctor has no business on the network for this. Over 30 days old, or no
/// cache at all, is a warning with the command that fixes it.
#[cfg(feature = "sessions")]
fn pricing_check(ctx: &Ctx) -> Value {
    const STALE_DAYS: i64 = 30;
    // The store's clock, which the golden cases can hold still.
    let now = krowk_store::now_ms();
    let fresh = crate::pricing::Freshness::of(ctx.io.env);
    let status = match fresh.age_days(now) {
        Some(d) if d <= STALE_DAYS => "pass",
        _ => "warn",
    };
    let mut v = json!({ "name": "pricing", "status": status, "message": fresh.describe(now) });
    if status == "warn" {
        v["hint"] = json!("run `krowk pricing refresh`, or `krowk sessions sync`, which refreshes prices once a day");
    }
    if let Some(d) = fresh.age_days(now) {
        v["age_days"] = json!(d);
    }
    v
}

#[cfg(not(feature = "sessions"))]
fn pricing_check(_: &Ctx) -> Value {
    json!({ "name": "pricing", "status": "skip", "message": "this build carries no prices" })
}

/// Whether any provider instance can run a turn here, by the readiness
/// check `krowk status` prints — each instance's state, and the command
/// with the sources and fixes. A warning, not a failure, when none is: a
/// machine that only pushes evidence needs no provider.
#[cfg(feature = "harness")]
fn providers_check(ctx: &Ctx) -> Value {
    let reports = match super::status::reports(ctx) {
        Ok((_, _, r)) => r,
        Err(e) => return json!({ "name": "providers", "status": "warn", "message": e.fix() }),
    };
    let ready: Vec<&str> = reports.iter().filter(|r| r.readiness.is_ready()).map(|r| r.instance.as_str()).collect();
    let message = match ready.as_slice() {
        [] => format!("none of {} instances is ready", reports.len()),
        r => format!("{} of {} instances ready: {}", r.len(), reports.len(), r.join(", ")),
    };
    let states: Map<String, Value> = reports.iter().map(|r| (r.instance.clone(), json!(r.readiness.state()))).collect();
    let mut v = json!({ "name": "providers", "status": if ready.is_empty() { "warn" } else { "pass" }, "message": message, "instances": states });
    v["hint"] = json!("run `krowk status` for where each key or login comes from, and what fixes it");
    v
}

/// The agent build runs no model, so it has no provider to check.
#[cfg(not(feature = "harness"))]
fn providers_check(_: &Ctx) -> Value {
    json!({ "name": "providers", "status": "skip", "message": "this build runs no model" })
}

fn registry_mode(ctx: &Ctx, client: &Client) -> &'static str {
    if client.base_url == krowk_api::DEV_BASE_URL.trim_end_matches('/') {
        "local"
    } else if !ctx.env("KROWK_API_URL").is_empty() {
        "custom (KROWK_API_URL)"
    } else {
        "production"
    }
}

/// The service descriptor at the root: needs no key, and names the service,
/// so a URL pointing at the website is caught too.
fn probe(client: &Client) -> String {
    match client.root() {
        Ok(s) if s.service.is_empty() => "reachable, but not a krowk registry".into(),
        Ok(s) => format!("reachable ({}, {})", s.service, s.versions.join(" ")),
        Err(e) if e.status != 0 => format!("reachable (HTTP {}) — {}", e.status, e.code()),
        Err(e) => match e.body.get("detail").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            Some(detail) => format!("unreachable — {} — {detail}", e.code()),
            None => format!("unreachable — {}", e.code()),
        },
    }
}

fn key_summary(client: &Client) -> String {
    if !client.authenticated() {
        return "none — uploads will be anonymous".into();
    }
    match client.verify_key() {
        Ok(k) if !k.name.is_empty() => format!("{} ({}) {}", k.key_id, k.workspace, k.name),
        Ok(k) => format!("{} ({})", k.key_id, k.workspace),
        Err(e) => format!("rejected — {}", e.code()),
    }
}

fn workspace_summary(ctx: &Ctx, resolved: &Result<(String, String), krowk_api::Error>) -> String {
    let (ws, source) = match resolved {
        Err(e) => return format!("unresolvable — {}", e.fix()),
        Ok(r) => r,
    };
    if !ws.is_empty() {
        if creds::read_token(ctx.io.env, ws).is_empty() {
            return format!("{ws} ({source}) — but no key is stored for it, so every command here fails");
        }
        if !ctx.env("KROWK_TOKEN").is_empty() {
            return format!("{ws} ({source}) — but KROWK_TOKEN is set and wins; uploads land wherever that key acts, and `krowk auth verify` names it");
        }
        return format!("{ws} ({source})");
    }
    if let Some(id) = creds::read_identity(ctx.io.env, "") {
        return format!("{} (stored default)", id.workspace);
    }
    if creds::token_source(ctx.io.env, "") == TOKEN_SOURCE_NONE {
        return "none — uploads will be anonymous".into();
    }
    "unknown — not recorded at login; `krowk auth verify` asks the registry".into()
}

fn config_summary() -> String {
    let existing = |p: std::path::PathBuf| if p.exists() { p.display().to_string() } else { format!("{} (absent)", p.display()) };
    let mut parts = vec![format!("global {}", existing(crate::config::global_path()))];
    if let Some(repo) = crate::config::repo_path("") {
        parts.push(format!("repo {}", existing(repo)));
    }
    parts.join(", ")
}
