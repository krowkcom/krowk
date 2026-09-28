//! The TUI's part of krowk's config.json, under `"tui"` (R-TUI-2):
//!
//! ```json
//! { "tui": { "statusBar": true, "statusItems": ["device", "model", "cost", "tasks", "subagents", "help"] } }
//! ```
//!
//! - `statusBar` — false hides the status line under the prompt. The "no
//!   network connectivity" notice is not part of it and shows regardless
//!   (R-OFF-1).
//! - `statusItems` — which items the status line shows, in order, joined by
//!   ` | `: any of `device` (`<user>/<host>`), `model` (the instance and
//!   model, `anthropic/claude-opus-5-5`, with the instance's limit once it
//!   is near it — R-INST-6), `cost` (the session's), `tasks` (`[2 tasks]`,
//!   only while the todo list has open items), `subagents` (`[1 subagent]`,
//!   only while subagents run) and `help` (`? help`, always last). The
//!   default is all six in that order. While the API cannot be reached an
//!   `offline` item is added before `help` whatever the list says.
//!
//! Names from before the status line was one template still read: `todos`
//! is `tasks`, `instance` is `model`, and `connectivity` and `session` are
//! taken and ignored — offline shows by itself, and the session's id is in
//! the details overlay.
//!
//! `/settings` (or `/config`) sets what lives outside `"tui"`: for now
//! `permissions.defaultMode`, cycled between `default` and `unhinged`, which
//! the next session starts in (`--permission-mode` and a trusted
//! repository's own `defaultMode` still come first).
//!
//! The overlays are toggled from the keyboard rather than configured: `?` on
//! an empty prompt for the keys, Ctrl-O for the session's details, Ctrl-T
//! for the todo list and Ctrl-G for the subagents.

use krowk_harness::protocol::PermissionMode;
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// `<user>/<short hostname>`, as it was when the TUI started.
    Device,
    /// `<instance>/<model>`, and the instance's limit once it is near.
    Model,
    Cost,
    /// `[N tasks]` while the todo list has pending or in-progress items.
    Tasks,
    /// `[N subagents]` while subagents run.
    Subagents,
    /// `? help`, always drawn last.
    Help,
}

impl Item {
    pub const ALL: [(&'static str, Item); 6] = [
        ("device", Item::Device),
        ("model", Item::Model),
        ("cost", Item::Cost),
        ("tasks", Item::Tasks),
        ("subagents", Item::Subagents),
        ("help", Item::Help),
    ];

    /// An item by name — `Some(None)` for an old name kept so a config that
    /// has it still reads, and that no longer shows anything of its own.
    fn parse(s: &str) -> Option<Option<Item>> {
        match s {
            "todos" => Some(Some(Item::Tasks)),
            "instance" => Some(Some(Item::Model)),
            "connectivity" | "session" => Some(None),
            _ => Item::ALL.iter().find(|(n, _)| *n == s).map(|(_, i)| Some(*i)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub status_bar: bool,
    pub status_items: Vec<Item>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { status_bar: true, status_items: Item::ALL.iter().map(|(_, i)| *i).collect() }
    }
}

/// Reads `"tui"` from a parsed config.json. Something written wrong is
/// reported, and the rest still applies: a typo in a status item should not
/// keep anybody from their prompt.
pub fn from_config(raw: &Value) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut warnings = Vec::new();
    let Some(tui) = raw.get("tui") else { return (s, warnings) };
    let Some(tui) = tui.as_object() else {
        warnings.push("config \"tui\" must be an object — using the defaults".into());
        return (s, warnings);
    };
    match tui.get("statusBar") {
        None => {}
        Some(Value::Bool(b)) => s.status_bar = *b,
        Some(_) => warnings.push("config tui.statusBar must be true or false".into()),
    }
    match tui.get("statusItems") {
        None => {}
        Some(Value::Array(items)) => {
            s.status_items.clear();
            for v in items {
                match v.as_str().and_then(Item::parse) {
                    Some(Some(i)) if !s.status_items.contains(&i) => s.status_items.push(i),
                    Some(_) => {}
                    None => warnings.push(format!(
                        "config tui.statusItems: {v} is not an item — one of {}",
                        Item::ALL.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                    )),
                }
            }
        }
        Some(_) => warnings.push("config tui.statusItems must be a list of item names".into()),
    }
    for key in tui.keys() {
        if !matches!(key.as_str(), "statusBar" | "statusItems") {
            warnings.push(format!("config tui.{key} is not a setting — the TUI reads statusBar and statusItems"));
        }
    }
    (s, warnings)
}

/// The modes `/settings` cycles the default through: the one that asks,
/// and the one that asks about nothing.
pub const DEFAULT_MODES: [PermissionMode; 2] = [PermissionMode::Default, PermissionMode::Unhinged];

/// `permissions.defaultMode` in krowk's config.json as written — none when
/// it names nothing, which runs as `default`.
pub fn default_mode(config: &Path) -> Result<Option<String>, String> {
    let raw = read(config)?;
    Ok(raw.get("permissions").and_then(|p| p.get("defaultMode")).and_then(Value::as_str).map(String::from))
}

/// The mode after `now` in `DEFAULT_MODES` (none is `default`); the first
/// for any other.
pub fn next_default(now: Option<&str>) -> PermissionMode {
    let now = now.unwrap_or(PermissionMode::Default.name());
    let at = DEFAULT_MODES.iter().position(|m| m.name() == now);
    at.map_or(DEFAULT_MODES[0], |i| DEFAULT_MODES[(i + 1) % DEFAULT_MODES.len()])
}

/// Writes `permissions.defaultMode`, keeping every other key as it was.
pub fn set_default_mode(config: &Path, m: PermissionMode) -> Result<(), String> {
    let mut raw = read(config)?;
    let permissions = raw.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Some(permissions) = permissions.as_object_mut() else {
        return Err(format!("\"permissions\" in {} is not an object — fix it by hand", config.display()));
    };
    permissions.insert("defaultMode".into(), Value::String(m.name().into()));
    krowk_harness::connect::write_config(config, &raw).map_err(|e| format!("{}: {e}", config.display()))
}

fn read(config: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read(config) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| format!("{} is not valid JSON: {e}", config.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("reading {}: {e}", config.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_cycles_the_default_mode_and_keeps_the_rest_of_the_config() {
        let dir = std::env::temp_dir().join(format!("krowk-tui-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("config.json");
        assert_eq!(default_mode(&config), Ok(None), "no file names nothing");
        assert_eq!(next_default(None), PermissionMode::Unhinged, "unset runs as default, so the next is unhinged");
        assert_eq!(next_default(Some("unhinged")), PermissionMode::Default);
        assert_eq!(next_default(Some("acceptEdits")), PermissionMode::Default, "a mode outside the cycle goes to its start");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&config, json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"]}}).to_string()).unwrap();
        set_default_mode(&config, PermissionMode::Unhinged).unwrap();
        assert_eq!(default_mode(&config), Ok(Some("unhinged".into())));
        let raw: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(raw, json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}}));
        std::fs::write(&config, json!({"permissions": true}).to_string()).unwrap();
        assert!(set_default_mode(&config, PermissionMode::Default).is_err(), "a permissions that is no object is not overwritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r_tui_2_the_status_bar_is_optional_and_its_items_configurable() {
        assert_eq!(from_config(&json!({})).0, Settings::default());
        assert_eq!(Settings::default().status_items, [Item::Device, Item::Model, Item::Cost, Item::Tasks, Item::Subagents, Item::Help], "the template's order");
        let (s, w) = from_config(&json!({"tui": {"statusBar": false}}));
        assert!(!s.status_bar && w.is_empty());
        let (s, w) = from_config(&json!({"tui": {"statusItems": ["cost", "model", "cost", "nope"]}}));
        assert_eq!(s.status_items, [Item::Cost, Item::Model], "in the order given, once each");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("\"nope\"") && w[0].contains("subagents"), "{w:?}");
        let (s, w) = from_config(&json!({"tui": {"statusBar": "yes", "colour": 1}}));
        assert!(s.status_bar, "a malformed value leaves the default");
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn r_tui_2_a_config_written_for_the_old_bar_still_reads() {
        let (s, w) = from_config(&json!({"tui": {"statusItems": ["model", "instance", "cost", "connectivity", "session", "todos", "subagents"]}}));
        assert!(w.is_empty(), "no warning for a name the bar once had: {w:?}");
        assert_eq!(s.status_items, [Item::Model, Item::Cost, Item::Tasks, Item::Subagents], "instance is model, todos is tasks, connectivity and session show nothing of their own");
    }
}
