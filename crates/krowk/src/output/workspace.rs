//! The stored keys, the configuration layers, and what each write did.

use super::{crumb, crumb_line, encode, ok, paint, success, warning, Breadcrumb, Format, DIM};
use krowk_api::creds::WorkspaceKey;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;

/// What `krowk workspaces` knows: the stored keys and which one resolves here.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Workspaces {
    pub stored: Vec<WorkspaceKey>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resolved: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(rename = "shadowed_by_env", skip_serializing_if = "std::ops::Not::not")]
    pub shadowed: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub key_missing: bool,
}

pub fn workspace_list(ws: &Workspaces, f: Format, quiet: bool, colour: bool) -> String {
    if f != Format::Human {
        return if quiet { encode(ws) } else { ok(ws, summary(ws), crumbs(ws)) };
    }
    if ws.stored.is_empty() {
        return format!(
            "No keys stored yet. Until there is one, uploads are anonymous and expire.\n{}",
            crumb_line("Add one", "krowk login", colour)
        );
    }
    let mut lines = vec!["Stored keys".to_string()];
    for k in &ws.stored {
        let mark = if k.default { format!("  {}", paint(colour, DIM, "(default)")) } else { String::new() };
        let name = if k.workspace_name.is_empty() { k.name.clone() } else { format!("{} — {}", k.workspace_name, k.name) };
        lines.push(format!("  {name:<40} {}{mark}", k.key_id));
    }
    if !ws.resolved.is_empty() && ws.key_missing {
        lines.push(warning(
            colour,
            &format!("{} resolves here ({}), but no key is stored for it, so every upload fails until `krowk login`", ws.resolved, ws.source),
        ));
    } else if !ws.resolved.is_empty() {
        lines.push(format!("Uploads from here land in {} {}", ws.resolved, paint(colour, DIM, &format!("({})", ws.source))));
    }
    if ws.shadowed {
        lines.push(warning(colour, "KROWK_TOKEN is set and wins over every stored key — uploads use that key instead"));
    }
    lines.join("\n")
}

fn summary(ws: &Workspaces) -> String {
    if ws.shadowed {
        return "KROWK_TOKEN is set and wins over every stored key — `krowk whoami` names the workspace that key acts in".into();
    }
    let stored = format!("{} key(s) stored", ws.stored.len());
    if ws.resolved.is_empty() {
        return if ws.stored.is_empty() {
            "no keys stored — uploads are anonymous".into()
        } else {
            format!("{stored}, nothing resolves here — uploads are anonymous")
        };
    }
    if ws.key_missing {
        return format!(
            "{} resolves here ({}) but holds no key — every upload fails until `krowk login`",
            ws.resolved, ws.source
        );
    }
    format!("uploads from here land in {} ({}) — {stored}", ws.resolved, ws.source)
}

fn crumbs(ws: &Workspaces) -> Vec<Breadcrumb> {
    if ws.stored.is_empty() {
        return vec![crumb(
            "log in",
            "krowk login",
            "approving it in the browser stores a key, and uploads stop expiring — `--token krowk_sk_...` stores one directly instead",
        )];
    }
    vec![
        crumb(
            "switch the default",
            "krowk workspaces use <workspace>",
            "repoints the machine-wide default at another stored key — <workspace> is a name from this list",
        ),
        crumb(
            "pin this repository",
            "krowk config set workspace <workspace>",
            "writes .krowk/config.json at the repository root, so every command run inside it uses that workspace's key \
             whoever runs it — <workspace> is a name from this list",
        ),
    ]
}

pub fn default_workspace(name: &str, path: &str, f: Format, quiet: bool, colour: bool) -> String {
    let r = json!({ "default": name, "path": path });
    if f != Format::Human {
        if quiet {
            return encode(&r);
        }
        return ok(
            r,
            format!("default workspace is now {name}"),
            vec![crumb(
                "check what resolves here",
                "krowk workspaces",
                "a repo config, KROWK_WORKSPACE or --workspace still outranks the default",
            )],
        );
    }
    success(colour, &format!("Default workspace is now {name}"))
}

/// The effective configuration and which layer set each value.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConfigView {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,
    pub global_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo_path: String,
}

pub fn config_show(v: &ConfigView, f: Format, quiet: bool, colour: bool) -> String {
    let source = v.sources.get("workspace").cloned().unwrap_or_default();
    if f != Format::Human {
        if quiet {
            return encode(v);
        }
        let summary = if v.workspace.is_empty() {
            "no configuration set — uploads use the stored default key".to_string()
        } else {
            format!("workspace {} — {source}", v.workspace)
        };
        return ok(
            v,
            summary,
            vec![crumb(
                "set the workspace",
                "krowk config set workspace <workspace>",
                "pins which workspace commands in this repository use — --global sets the machine-wide fallback instead",
            )],
        );
    }
    let mut lines = vec![if v.workspace.is_empty() {
        paint(colour, DIM, "Nothing set — uploads use the stored default key.")
    } else {
        format!("{:<11} {}  {}", "Workspace", v.workspace, paint(colour, DIM, &format!("({source})")))
    }];
    lines.push(format!("  {:<9} {}", "Global", v.global_path));
    if !v.repo_path.is_empty() {
        lines.push(format!("  {:<9} {}", "Repo", v.repo_path));
    }
    lines.join("\n")
}

pub fn config_wrote(key: &str, value: &str, path: &str, f: Format, quiet: bool, colour: bool) -> String {
    let mut r = serde_json::Map::new();
    r.insert("key".into(), json!(key));
    if !value.is_empty() {
        r.insert("value".into(), json!(value));
    }
    r.insert("path".into(), json!(path));
    let said = if value.is_empty() { format!("{key} removed from {path}") } else { format!("{key} = {value} in {path}") };
    if f != Format::Human {
        if quiet {
            return encode(&r);
        }
        return ok(
            r,
            said,
            vec![crumb(
                "check the effective configuration",
                "krowk config show",
                "reports what actually resolves here, which layer wins, and why",
            )],
        );
    }
    success(colour, &said)
}
