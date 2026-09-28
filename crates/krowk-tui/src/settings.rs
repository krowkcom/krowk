//! The TUI's part of krowk's config.json, under `"tui"` (R-TUI-2):
//!
//! ```json
//! { "tui": { "contentWidth": "prose", "statusBar": true, "statusItems": ["device", "model", "cost", "tasks", "subagents", "help"] } }
//! ```
//!
//! - `contentWidth` — `prose` (the default) lays everything out at most 65
//!   columns wide, Tailwind's `max-w-prose` (`65ch`), however wide the
//!   terminal; a narrower one still gets all of its width. `full-width`
//!   takes the terminal's whole width.
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
//! `/settings` (or `/config`) sets, chosen with ↑ and ↓ and changed with ←
//! and →, `permissions.defaultMode` — `default` or `unhinged`, which the next
//! session starts in (`--permission-mode` and a trusted repository's own
//! `defaultMode` still come first) — and `tui.contentWidth`, which applies
//! at once.
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

/// How wide the TUI lays out, inside its padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentWidth {
    /// At most `PROSE` columns.
    #[default]
    Prose,
    /// The terminal's whole width.
    FullWidth,
}

impl ContentWidth {
    /// In the order `/settings` steps through them.
    pub const ALL: [(&'static str, ContentWidth); 2] = [("prose", ContentWidth::Prose), ("full-width", ContentWidth::FullWidth)];

    /// Tailwind's `max-w-prose`, `65ch`: 65 columns of a monospace font.
    pub const PROSE: u16 = 65;

    pub fn name(self) -> &'static str {
        ContentWidth::ALL.iter().find(|(_, w)| *w == self).map_or("prose", |(n, _)| n)
    }

    fn parse(s: &str) -> Option<ContentWidth> {
        ContentWidth::ALL.iter().find(|(n, _)| *n == s).map(|(_, w)| *w)
    }

    /// The width to lay out in, given `room`: a maximum, never more than
    /// there is.
    pub fn of(self, room: u16) -> u16 {
        match self {
            ContentWidth::Prose => room.min(ContentWidth::PROSE),
            ContentWidth::FullWidth => room,
        }
    }

    /// The width `by` along `ALL` from this one, and none past either end:
    /// the same key again chooses nothing new.
    pub fn step(self, by: isize) -> Option<ContentWidth> {
        let at = ContentWidth::ALL.iter().position(|(_, w)| *w == self).unwrap_or(0);
        at.checked_add_signed(by).and_then(|i| ContentWidth::ALL.get(i)).map(|(_, w)| *w)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub content_width: ContentWidth,
    pub status_bar: bool,
    pub status_items: Vec<Item>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { content_width: ContentWidth::default(), status_bar: true, status_items: Item::ALL.iter().map(|(_, i)| *i).collect() }
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
    match tui.get("contentWidth") {
        None => {}
        Some(v) => match v.as_str().and_then(ContentWidth::parse) {
            Some(w) => s.content_width = w,
            None => warnings.push(format!("config tui.contentWidth: {v} is not a width — prose or full-width")),
        },
    }
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
        if !matches!(key.as_str(), "contentWidth" | "statusBar" | "statusItems") {
            warnings.push(format!("config tui.{key} is not a setting — the TUI reads contentWidth, statusBar and statusItems"));
        }
    }
    (s, warnings)
}

/// The modes `/settings` chooses the default from, left to right: the one
/// that asks, and the one that asks about nothing.
pub const DEFAULT_MODES: [PermissionMode; 2] = [PermissionMode::Default, PermissionMode::Unhinged];

/// `permissions.defaultMode` in a read config.json as written — none when
/// there is none (or null), which runs as `default`; a value that is no
/// string as its JSON.
pub fn default_mode(raw: &Map<String, Value>) -> Option<String> {
    raw.get("permissions").and_then(|p| p.get("defaultMode")).filter(|v| !v.is_null()).map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))
}

/// The mode `by` along `DEFAULT_MODES` from `now` (none is `default`), and
/// none past either end: the same key again chooses nothing new. From a
/// mode outside them, either way is the first.
pub fn step_default(now: Option<&str>, by: isize) -> Option<PermissionMode> {
    let now = now.unwrap_or(PermissionMode::Default.name());
    let Some(at) = DEFAULT_MODES.iter().position(|m| m.name() == now) else { return Some(DEFAULT_MODES[0]) };
    at.checked_add_signed(by).and_then(|i| DEFAULT_MODES.get(i)).copied()
}

/// Writes `permissions.defaultMode`, keeping every other key as it was:
/// config.json as written.
pub fn set_default_mode(config: &Path, m: PermissionMode) -> Result<Map<String, Value>, String> {
    let mut raw = krowk_harness::connect::read_config(config)?;
    let permissions = raw.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Some(permissions) = permissions.as_object_mut() else {
        return Err(format!("\"permissions\" in {} is not an object — fix it by hand", config.display()));
    };
    permissions.insert("defaultMode".into(), Value::String(m.name().into()));
    krowk_harness::connect::write_config(config, &raw).map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(raw)
}

/// Writes `tui.contentWidth`, keeping every other key as it was.
pub fn set_content_width(config: &Path, w: ContentWidth) -> Result<Map<String, Value>, String> {
    let mut raw = krowk_harness::connect::read_config(config)?;
    let tui = raw.entry("tui").or_insert_with(|| Value::Object(Map::new()));
    let Some(tui) = tui.as_object_mut() else {
        return Err(format!("\"tui\" in {} is not an object — fix it by hand", config.display()));
    };
    tui.insert("contentWidth".into(), Value::String(w.name().into()));
    krowk_harness::connect::write_config(config, &raw).map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(raw)
}

/// The mode a session started in `cwd` would run in with `raw` as krowk's
/// config.json, when a file read after it — `~/.claude/settings.json`, a
/// trusted repository's — sets another; none when config.json's is the
/// one, or the settings do not load (starting says why).
pub fn overridden(cfg: &krowk_harness::permissions::Config, raw: &Map<String, Value>, cwd: &Path) -> Option<PermissionMode> {
    // Only the mode is compared: the rest is what starting reads and says.
    let cfg = krowk_harness::permissions::Config { user: Some(Value::Object(raw.clone())), ..cfg.clone() };
    let runs = krowk_harness::permissions::settings::load(&cfg, cwd).ok()?.default_mode.unwrap_or_default();
    let saved = default_mode(raw).and_then(|m| PermissionMode::parse(&m)).unwrap_or_default();
    (runs != saved).then_some(runs)
}

/// Claude Code's user settings file, `~` for the home directory:
/// `$CLAUDE_CONFIG_DIR/settings.json` when that is set.
pub fn claude_file(cfg: &krowk_harness::permissions::Config) -> String {
    let Some(file) = cfg.claude_home().map(|d| d.join("settings.json")) else { return "~/.claude/settings.json".into() };
    match cfg.home.as_deref().and_then(|h| file.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => file.display().to_string(),
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
        assert_eq!(krowk_harness::connect::read_config(&config).map(|r| default_mode(&r)), Ok(None), "no file names nothing");
        assert_eq!(step_default(None, 1), Some(PermissionMode::Unhinged), "unset runs as default, so → is unhinged");
        assert_eq!(step_default(None, -1), None, "nothing left of default");
        assert_eq!(step_default(Some("unhinged"), 1), None, "held →: nothing past unhinged");
        assert_eq!(step_default(Some("unhinged"), -1), Some(PermissionMode::Default));
        assert_eq!(step_default(Some("acceptEdits"), 1), Some(PermissionMode::Default), "a mode outside them goes to the first");
        assert_eq!(default_mode(json!({"permissions": {"defaultMode": true}}).as_object().unwrap()).as_deref(), Some("true"), "no string is shown as it is");
        assert_eq!(default_mode(json!({"permissions": {"defaultMode": null}}).as_object().unwrap()), None, "null is none, as the harness reads it");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&config, json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"]}}).to_string()).unwrap();
        let written = set_default_mode(&config, PermissionMode::Unhinged).unwrap();
        assert_eq!(default_mode(&written).as_deref(), Some("unhinged"));
        assert_eq!(krowk_harness::connect::read_config(&config).unwrap(), written);
        let raw: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(raw, json!({"tui": {"statusBar": false}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}}));
        let written = set_content_width(&config, ContentWidth::FullWidth).unwrap();
        assert_eq!(Value::Object(written), json!({"tui": {"statusBar": false, "contentWidth": "full-width"}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}}));
        std::fs::write(&config, json!({"permissions": true, "tui": []}).to_string()).unwrap();
        assert!(set_content_width(&config, ContentWidth::Prose).is_err(), "a tui that is no object is not overwritten");
        assert!(set_default_mode(&config, PermissionMode::Default).is_err(), "a permissions that is no object is not overwritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_names_the_mode_a_file_read_after_config_json_puts_a_session_in() {
        let dir = std::env::temp_dir().join(format!("krowk-tui-settings-over-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let claude = dir.join("claude");
        std::fs::create_dir_all(&claude).unwrap();
        let cfg = krowk_harness::permissions::Config { home: Some(dir.clone()), claude_dir: Some(claude.clone()), ..Default::default() };
        let raw = |m: &str| json!({"permissions": {"defaultMode": m}}).as_object().unwrap().clone();
        assert_eq!(overridden(&cfg, &raw("unhinged"), &dir), None, "nothing else sets one");
        std::fs::write(claude.join("settings.json"), json!({"permissions": {"defaultMode": "acceptEdits"}}).to_string()).unwrap();
        assert_eq!(overridden(&cfg, &raw("unhinged"), &dir), Some(PermissionMode::AcceptEdits), "Claude's user file comes after config.json");
        assert_eq!(overridden(&cfg, &raw("acceptEdits"), &dir), None, "the same mode overrides nothing");
        assert_eq!(claude_file(&cfg), "~/claude/settings.json", "CLAUDE_CONFIG_DIR's, under home as ~");
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
    fn the_content_width_is_a_maximum_prose_by_default() {
        assert_eq!(Settings::default().content_width, ContentWidth::Prose);
        assert_eq!(ContentWidth::Prose.of(200), 65, "Tailwind's max-w-prose");
        assert_eq!(ContentWidth::Prose.of(40), 40, "a narrower terminal keeps all of its width");
        assert_eq!(ContentWidth::FullWidth.of(200), 200);
        assert_eq!(from_config(&json!({"tui": {"contentWidth": "full-width"}})).0.content_width, ContentWidth::FullWidth);
        let (s, w) = from_config(&json!({"tui": {"contentWidth": "wide"}}));
        assert_eq!(s.content_width, ContentWidth::Prose, "a malformed value leaves the default");
        assert!(w.len() == 1 && w[0].contains("\"wide\"") && w[0].contains("full-width"), "{w:?}");
        assert_eq!(ContentWidth::Prose.step(1), Some(ContentWidth::FullWidth));
        assert_eq!(ContentWidth::FullWidth.step(1), None, "held →: nothing past full-width");
        assert_eq!(ContentWidth::Prose.step(-1), None);
    }

    #[test]
    fn r_tui_2_a_config_written_for_the_old_bar_still_reads() {
        let (s, w) = from_config(&json!({"tui": {"statusItems": ["model", "instance", "cost", "connectivity", "session", "todos", "subagents"]}}));
        assert!(w.is_empty(), "no warning for a name the bar once had: {w:?}");
        assert_eq!(s.status_items, [Item::Model, Item::Cost, Item::Tasks, Item::Subagents], "instance is model, todos is tasks, connectivity and session show nothing of their own");
    }
}
