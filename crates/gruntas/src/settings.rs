//! The two settings only the terminal UI could change, now `gruntas config`'s:
//! the default permission mode (`permissions.defaultMode`, nested, as the
//! engine reads it) and the rollover (`rollover`). Both live in the global
//! config.json; a pre-existing key this build does not know stays as written.

use gruntas_harness::protocol::{PermissionMode, Rollover};
use serde_json::{Map, Value};
use std::path::Path;

/// The modes the default can be set to: the one that asks, and the one that
/// asks about nothing.
pub const DEFAULT_MODES: [PermissionMode; 2] = [PermissionMode::Default, PermissionMode::Unhinged];

/// A mode as typed, or the refusal naming the valid ones.
pub fn parse_default_mode(s: &str) -> Result<PermissionMode, String> {
    DEFAULT_MODES
        .into_iter()
        .find(|m| m.name() == s.trim())
        .ok_or_else(|| format!("defaultMode {:?} is not a value — one of {}", s.trim(), DEFAULT_MODES.map(|m| m.name()).join(", ")))
}

/// `permissions.defaultMode` in a read config.json as written — none when
/// there is none (or null), which runs as `default`; a value that is no
/// string as its JSON.
pub fn default_mode(raw: &Map<String, Value>) -> Option<String> {
    raw.get("permissions").and_then(|p| p.get("defaultMode")).filter(|v| !v.is_null()).map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))
}

/// Writes `permissions.defaultMode`, keeping every other key as it was.
pub fn set_default_mode(config: &Path, m: PermissionMode) -> Result<Map<String, Value>, String> {
    let mut raw = gruntas_harness::connect::read_config(config)?;
    let permissions = raw.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Some(permissions) = permissions.as_object_mut() else {
        return Err(format!("\"permissions\" in {} is not an object — fix it by hand", config.display()));
    };
    permissions.insert("defaultMode".into(), Value::String(m.name().into()));
    gruntas_harness::connect::write_config(config, &raw).map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(raw)
}

/// A rollover value as it is written in config.json.
pub fn rollover_name(r: Rollover) -> &'static str {
    match r {
        Rollover::Auto => "auto",
        Rollover::Offer => "offer",
        Rollover::Off => "off",
    }
}

/// `rollover` in a read config.json as written — none when there is none
/// (or null); a value that is no string as its JSON.
pub fn global_rollover(raw: &Map<String, Value>) -> Option<String> {
    raw.get("rollover").filter(|v| !v.is_null()).map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))
}

/// Writes the global `rollover`, keeping every other key as it was.
pub fn set_rollover(config: &Path, r: Rollover) -> Result<Map<String, Value>, String> {
    let mut raw = gruntas_harness::connect::read_config(config)?;
    raw.insert("rollover".into(), Value::String(rollover_name(r).into()));
    gruntas_harness::connect::write_config(config, &raw).map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(raw)
}

/// The rollover a session started here runs with and where it comes from:
/// a trusted repository's, else config.json's, else the default. (A
/// session's own value lives only in its log.)
pub fn effective_rollover(repo: Option<Rollover>, global: Option<Rollover>) -> (Rollover, &'static str) {
    match (repo, global) {
        (Some(r), _) => (r, "repo"),
        (None, Some(r)) => (r, "global"),
        (None, None) => (Rollover::default(), "default"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rollover_resolves_repo_then_global_then_default() {
        assert_eq!(effective_rollover(Some(Rollover::Off), Some(Rollover::Auto)), (Rollover::Off, "repo"));
        assert_eq!(effective_rollover(None, Some(Rollover::Auto)), (Rollover::Auto, "global"));
        assert_eq!(effective_rollover(None, None), (Rollover::Offer, "default"));
    }

    #[test]
    fn writes_keep_every_other_key() {
        let dir = std::env::temp_dir().join(format!("gruntas-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, r#"{"tui":{"statusBar":false},"permissions":{"allow":["Bash(ls)"]},"rolloverOrder":["a"]}"#).unwrap();
        set_rollover(&config, Rollover::Off).unwrap();
        let written = set_default_mode(&config, PermissionMode::Unhinged).unwrap();
        assert_eq!(default_mode(&written).as_deref(), Some("unhinged"));
        assert_eq!(global_rollover(&written).as_deref(), Some("off"));
        let raw: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(raw, json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}, "rolloverOrder": ["a"], "rollover": "off"}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_default_mode_outside_the_two_is_refused_naming_them() {
        assert_eq!(parse_default_mode("unhinged"), Ok(PermissionMode::Unhinged));
        let e = parse_default_mode("plan").unwrap_err();
        assert!(e.contains("default, unhinged"), "{e}");
    }
}
