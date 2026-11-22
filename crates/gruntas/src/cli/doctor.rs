//! `krowk doctor`: describes a broken setup rather than being stopped by one,
//! so the workspace is resolved by hand and every check reports what it found.

use super::health::{self, Check, Lookup};
use super::workspace::resolve_workspace;
use super::{Ctx, VERSION};
use crate::output::Format;
use gruntas_api::creds::{self, TOKEN_SOURCE_NONE};
use gruntas_api::Client;
use serde_json::{json, Map, Value};


pub(crate) fn doctor(ctx: &mut Ctx) -> Result<(), gruntas_api::Error> {
    let resolved = resolve_workspace(ctx);
    let ws = resolved.as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
    let client = Client::new(&gruntas_api::base_url_for(ctx.f.dev, ctx.io.env), &creds::read_token(ctx.io.env, &ws));

    let mut report = Map::new();
    report.insert("version".into(), json!(VERSION));
    report.insert("runtime".into(), json!(format!("rust {}/{}", std::env::consts::OS, std::env::consts::ARCH)));
    report.insert("api".into(), json!(client.base_url));
    report.insert("registry".into(), json!(registry_mode(ctx, &client)));
    report.insert("api_status".into(), json!(probe(&client)));
    report.insert("authenticated".into(), json!(client.authenticated()));
    report.insert("token_source".into(), json!(creds::token_source(ctx.io.env, &ws)));
    let signed_in = client.authenticated();
    let verified = signed_in.then(|| client.verify_key());
    report.insert("key".into(), json!(key_summary(&verified)));
    report.insert("workspace".into(), json!(workspace_summary(ctx, &resolved)));
    // Runs are where metadata goes, and they need a key.
    report.insert("runs_available".into(), json!(client.authenticated()));
    report.insert("credentials".into(), json!(creds::credentials_text()));
    report.insert("config".into(), json!(config_summary()));
    report.insert("store".into(), store_check(ctx));
    report.insert("pricing".into(), pricing_check(ctx));
    report.insert("providers".into(), providers_check(ctx));
    #[cfg(feature = "harness")]
    report.insert("rollover".into(), rollover_check(ctx));
    report.insert("context".into(), serde_json::to_value(crate::runctx::detect(ctx.io.env)).expect("context serializes"));
    let checks = all_checks(ctx, &report, signed_in, &verified);
    let failing = checks.iter().filter(|c| c["status"] == "fail").count();
    report.insert("checks".into(), Value::Array(checks));

    if ctx.format != Format::Human {
        ctx.emit(&crate::output::encode(&report))?;
    } else {
        let text = human(&report, ctx.colour);
        let _ = writeln!(ctx.io.stdout, "{text}");
    }
    // Exits non-zero exactly when a check failed: a warning is worth knowing,
    // and is no failure.
    match failing {
        0 => Ok(()),
        n => Err(gruntas_api::fail("doctor_failed", format!("{} failed — each ✗ line says what fixes it", if n == 1 { "1 check".to_string() } else { format!("{n} checks") }))),
    }
}

/// Every check, in the order of importance, each as `name`, `status`,
/// `message` and — for anything that did not pass — the `fix`.
fn all_checks(ctx: &Ctx, report: &Map<String, Value>, signed_in: bool, verified: &Option<Result<gruntas_api::types::Key, gruntas_api::Error>>) -> Vec<Value> {
    let lookup = match verified {
        None => Lookup::Unchecked,
        Some(Ok(k)) => Lookup::Ok { email: k.email.clone(), workspace: k.workspace.clone() },
        // A key is bad when the registry says so; any other answer (down,
        // slow, rate-limited) leaves it neither good nor bad.
        Some(Err(e)) if matches!(e.status, 401 | 403) => Lookup::Rejected(e.code()),
        Some(Err(_)) => Lookup::Offline,
    };
    let (mut ours, machine) = health::local(ctx, signed_in, &lookup);
    let plan = verified.as_ref().and_then(|v| v.as_ref().ok()).map(|k| k.plan.as_str());
    ours.extend([health::trusted(&health::trust_here(ctx)), health::recovery_kit(&machine), health::plan(plan, &machine)]);
    let mut out: Vec<Value> = ours.iter().map(Check::json).collect();
    // The older checks, which carry a `hint` for the fix.
    let mut older = ["providers", "store", "pricing", "rollover"].into_iter().filter_map(|k| report.get(k)).map(|c| {
        let mut c = c.clone();
        if c["status"] != "pass"
            && let Some(hint) = c.get("hint").cloned()
        {
            c["fix"] = hint;
        }
        c
    });
    // The models come before this directory's trust, the rest after.
    let at = out.iter().position(|c| c["name"] == "trust").unwrap_or(out.len());
    out.splice(at..at, older.next());
    out.extend(older);
    out
}

/// The report for a person: the facts as a table, then each check with a
/// mark for how it went and, when it did not pass, what fixes it.
fn human(report: &Map<String, Value>, colour: bool) -> String {
    use crate::output::{paint, DIM};
    const FACTS: [(&str, &str); 12] = [
        ("version", "Version"),
        ("runtime", "Runtime"),
        ("api", "API"),
        ("registry", "Registry"),
        ("api_status", "API status"),
        ("authenticated", "Signed in"),
        ("token_source", "Key from"),
        ("key", "Key"),
        ("workspace", "Workspace"),
        ("runs_available", "Runs"),
        ("credentials", "Credentials"),
        ("config", "Config"),
    ];
    let mut lines: Vec<String> = FACTS
        .iter()
        .map(|(k, label)| {
            let v = match &report[*k] {
                Value::String(s) => s.clone(),
                Value::Bool(true) => "yes".into(),
                Value::Bool(false) => "no".into(),
                other => other.to_string(),
            };
            format!("{label:<15} {v}")
        })
        .collect();
    lines.push(String::new());
    let mark = |status: &str| match status {
        "pass" => paint(colour, crate::output::GREEN, "✓"),
        "skip" => paint(colour, DIM, "-"),
        "warn" => paint(colour, crate::output::YELLOW, "!"),
        _ => paint(colour, crate::output::RED, "✗"),
    };
    for check in report.get("checks").and_then(Value::as_array).into_iter().flatten() {
        let text = |key: &str| check.get(key).and_then(Value::as_str).unwrap_or_default();
        let name = text("name");
        let mark = mark(text("status"));
        let fix = crate::output::fix::sentence(text("fix"));
        // The older checks keep their label and a line for the fix; the rest
        // read as a sentence with the command after a dash.
        match [("store", "Store"), ("pricing", "Pricing"), ("providers", "Providers"), ("rollover", "Rollover")].iter().find(|(n, _)| *n == name) {
            Some((_, label)) => {
                lines.push(format!("{mark} {label:<13} {}", crate::output::fix::capitalised(text("message"))));
                if !text("fix").is_empty() {
                    lines.push(paint(colour, DIM, &format!("  {:<13} {fix}", "")));
                }
            }
            None if text("fix").is_empty() => lines.push(format!("{mark} {}", text("message"))),
            None => lines.push(format!("{mark} {} — {}", text("message"), text("fix"))),
        }
    }
    if let Some(Value::Object(context)) = report.get("context").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())) {
        let said: Vec<String> = context.iter().map(|(k, v)| format!("{k}={}", v.as_str().map_or_else(|| v.to_string(), str::to_string))).collect();
        lines.push(String::new());
        lines.push(format!("{:<15} {}", "Detected", said.join(" · ")));
    }
    lines.join("\n")
}

#[cfg(feature = "sessions")]
fn store_check(ctx: &Ctx) -> Value {
    serde_json::to_value(gruntas_store::check(ctx.io.env)).expect("check serializes")
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
    let now = gruntas_store::now_ms();
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
    v["hint"] = json!("run `krowk connect` to add one, or `krowk status` for where each key or login comes from");
    v
}

/// The agent build runs no model, so it has no provider to check.
#[cfg(not(feature = "harness"))]
fn providers_check(_: &Ctx) -> Value {
    json!({ "name": "providers", "status": "skip", "message": "this build runs no model" })
}

/// What rollover ignores in `rolloverOrder` (D5): a warning naming each
/// entry, so a list written for an older rule is not silently half-used.
#[cfg(feature = "harness")]
fn rollover_check(ctx: &Ctx) -> Value {
    let ignored = match super::status::reports(ctx) {
        Ok((_, registry, _)) => registry.ignored_rollover_entries(),
        Err(e) => return json!({ "name": "rollover", "status": "warn", "message": e.fix() }),
    };
    if ignored.is_empty() {
        return json!({ "name": "rollover", "status": "pass", "message": "rolloverOrder has nothing ignored" });
    }
    json!({ "name": "rollover", "status": "warn", "message": ignored.join("; "), "ignored": ignored, "hint": "edit rolloverOrder in ~/.krowk/config.json: list instances of one kind, without a model" })
}

fn registry_mode(ctx: &Ctx, client: &Client) -> &'static str {
    if client.base_url == gruntas_api::DEV_BASE_URL.trim_end_matches('/') {
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

fn key_summary(verified: &Option<Result<gruntas_api::types::Key, gruntas_api::Error>>) -> String {
    match verified {
        None => "none — uploads will be anonymous".into(),
        Some(Ok(k)) if !k.name.is_empty() => format!("{} ({}) {}", k.key_id, k.workspace, k.name),
        Some(Ok(k)) => format!("{} ({})", k.key_id, k.workspace),
        Some(Err(e)) => format!("rejected — {}", e.code()),
    }
}

fn workspace_summary(ctx: &Ctx, resolved: &Result<(String, String), gruntas_api::Error>) -> String {
    let (ws, source) = match resolved {
        Err(e) => return format!("unresolvable — {}", e.fix()),
        Ok(r) => r,
    };
    if !ws.is_empty() {
        if creds::read_token(ctx.io.env, ws).is_empty() {
            return format!("{ws} ({source}) — but no key is stored for it, so every command here fails");
        }
        if !ctx.env("KROWK_TOKEN").is_empty() {
            return format!("{ws} ({source}) — but KROWK_TOKEN is set and wins; uploads land wherever that key acts, and `krowk whoami` names it");
        }
        return format!("{ws} ({source})");
    }
    if let Some(id) = creds::read_identity(ctx.io.env, "") {
        return format!("{} (stored default)", id.workspace);
    }
    if creds::token_source(ctx.io.env, "") == TOKEN_SOURCE_NONE {
        return "none — uploads will be anonymous".into();
    }
    "unknown — not recorded at login; `krowk whoami` asks the registry".into()
}

fn config_summary() -> String {
    let existing = |p: std::path::PathBuf| if p.exists() { p.display().to_string() } else { format!("{} (absent)", p.display()) };
    let mut parts = vec![match crate::config::global_path() {
        Ok(p) => format!("global {}", existing(p)),
        Err(_) => "global (no home directory)".into(),
    }];
    if let Some(repo) = crate::config::repo_path("") {
        parts.push(format!("repo {}", existing(repo)));
    }
    parts.join(", ")
}
