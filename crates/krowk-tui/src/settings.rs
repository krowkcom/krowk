//! The TUI's part of krowk's config.json, under `"tui"` (R-TUI-2):
//!
//! ```json
//! { "tui": { "screen": "auto", "contentWidth": "prose", "statusBar": true, "statusItems": ["model", "device", "tasks", "subagents", "help", "branch", "pr", "cost"] } }
//! ```
//!
//! - `screen` — `auto` (the default) is `fullscreen`, but `inline` inside
//!   Zellij, whose own panes take the mouse and the alternate screen badly
//!   (as Grok Build decides). `fullscreen` takes the terminal's alternate
//!   screen: the prompt and the status line stay on the bottom rows while
//!   the conversation scrolls above them with the mouse wheel or PgUp and
//!   PgDn, a drag over it selects and copies, and on the way out the
//!   conversation is printed onto the shell's screen. `inline` draws at the bottom of the normal screen and leaves
//!   the conversation in the terminal's own scrollback (`term`), for a
//!   terminal where the alternate screen or the mouse is unwelcome. Read
//!   when the TUI starts.
//! - `contentWidth` — `prose` (the default) lays out what is above the
//!   prompt at most 80 columns wide, however wide the terminal; a narrower
//!   one still gets all of its width. `prose-wide` is the same at most 120
//!   columns, and `full-width` takes the terminal's whole width. The prompt and the status line take the whole width
//!   either way, and so does a code block's band: its rows are left for
//!   the terminal to wrap, so a mouse selection of a long line of code
//!   copies it whole.
//! - `statusBar` — false hides the status line under the prompt. The "no
//!   network connectivity" notice is not part of it and shows regardless
//!   (R-OFF-1).
//! - `statusItems` — which items the status line shows, in order, joined by
//!   ` | `: any of `model` (the instance and model,
//!   `anthropic/claude-opus-5-5`, with the instance's limit once it is near
//!   it — R-INST-6), `device` (`<user>/<host>`), `tasks` (`[2 tasks]`, only
//!   while the todo list has open items), `subagents` (`[1 subagent]`, only
//!   while subagents run), `help` (`? help`, last on its row), `branch` (the
//!   one checked out), `pr` (`#133↗`, the branch's pull request, a link
//!   coloured by its state) and `cost` (the session's). `branch`, `pr` and
//!   `cost` make the second row, the rest the first. The default is all
//!   eight in that order. While the API cannot be reached
//!   an `offline` item is added before `help` whatever the list says.
//!
//! Names from before the status line was one template still read: `todos`
//! is `tasks`, `instance` is `model`, and `connectivity` and `session` are
//! taken and ignored — offline shows by itself, and the session's id is in
//! the details overlay.
//!
//! `/settings` (or `/config`) sets, chosen with ↑ and ↓ and changed with ←
//! and →, `permissions.defaultMode` — `default` or `unhinged`, which the next
//! session starts in (`--permission-mode` and a trusted repository's own
//! `defaultMode` still come first) — `tui.contentWidth`, which applies at
//! once, and `tui.screen`, which krowk opens on the next time it starts
//! (`/new` keeps the terminal it has).
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
    /// `? help`, drawn last on the first row.
    Help,
    /// The branch checked out.
    Branch,
    /// `#N↗`, the pull request of the branch checked out.
    Pr,
}

impl Item {
    pub const ALL: [(&'static str, Item); 8] = [
        ("model", Item::Model),
        ("device", Item::Device),
        ("tasks", Item::Tasks),
        ("subagents", Item::Subagents),
        ("help", Item::Help),
        ("branch", Item::Branch),
        ("pr", Item::Pr),
        ("cost", Item::Cost),
    ];

    /// Whether the item is drawn on the status line's second row.
    pub fn second_row(self) -> bool {
        matches!(self, Item::Branch | Item::Pr | Item::Cost)
    }

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

/// How wide what is above the prompt lays out, inside the padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentWidth {
    /// At most `PROSE` columns.
    #[default]
    Prose,
    /// At most `PROSE_WIDE` columns.
    ProseWide,
    /// The terminal's whole width.
    FullWidth,
}

impl ContentWidth {
    /// In the order `/settings` steps through them.
    pub const ALL: [(&'static str, ContentWidth); 3] =
        [("prose", ContentWidth::Prose), ("prose-wide", ContentWidth::ProseWide), ("full-width", ContentWidth::FullWidth)];

    /// Wide enough for a line of code or a table row, narrow enough to read.
    pub const PROSE: u16 = 80;

    /// Room for a wide table or a long line of code, still short of a wide
    /// terminal's whole width.
    pub const PROSE_WIDE: u16 = 120;

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
            ContentWidth::ProseWide => room.min(ContentWidth::PROSE_WIDE),
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

/// Where the TUI draws (`tui.screen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// Fullscreen, but inline inside Zellij.
    #[default]
    Auto,
    /// The alternate screen, the footer pinned (`crate::full`).
    Fullscreen,
    /// The normal screen, the conversation in its scrollback (`crate::term`).
    Inline,
}

impl Screen {
    /// In the order `/settings` steps through them.
    pub const ALL: [(&'static str, Screen); 3] = [("auto", Screen::Auto), ("fullscreen", Screen::Fullscreen), ("inline", Screen::Inline)];

    pub fn name(self) -> &'static str {
        Screen::ALL.iter().find(|(_, s)| *s == self).map_or("auto", |(n, _)| n)
    }

    /// The screen `by` along `ALL` from this one, and none past either end.
    pub fn step(self, by: isize) -> Option<Screen> {
        let at = Screen::ALL.iter().position(|(_, s)| *s == self).unwrap_or(0);
        at.checked_add_signed(by).and_then(|i| Screen::ALL.get(i)).map(|(_, s)| *s)
    }

    /// Whether the TUI takes the alternate screen, in the environment
    /// `env` reads.
    pub fn fullscreen(self, env: &dyn Fn(&str) -> String) -> bool {
        match self {
            Screen::Auto => env("ZELLIJ").is_empty(),
            Screen::Fullscreen => true,
            Screen::Inline => false,
        }
    }

    fn parse(s: &str) -> Option<Screen> {
        Screen::ALL.iter().find(|(n, _)| *n == s).map(|(_, m)| *m)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub screen: Screen,
    pub content_width: ContentWidth,
    pub status_bar: bool,
    pub status_items: Vec<Item>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { screen: Screen::default(), content_width: ContentWidth::default(), status_bar: true, status_items: Item::ALL.iter().map(|(_, i)| *i).collect() }
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
    match tui.get("screen") {
        None => {}
        Some(v) => match v.as_str().and_then(Screen::parse) {
            Some(m) => s.screen = m,
            None => warnings.push(format!("config tui.screen: {v} is not a screen — auto, fullscreen or inline")),
        },
    }
    match tui.get("contentWidth") {
        None => {}
        Some(v) => match v.as_str().and_then(ContentWidth::parse) {
            Some(w) => s.content_width = w,
            None => warnings.push(format!("config tui.contentWidth: {v} is not a width — prose, prose-wide or full-width")),
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
        if !matches!(key.as_str(), "screen" | "contentWidth" | "statusBar" | "statusItems") {
            warnings.push(format!("config tui.{key} is not a setting — the TUI reads screen, contentWidth, statusBar and statusItems"));
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
    set_tui(config, "contentWidth", w.name())
}

/// Writes `tui.screen`, keeping every other key as it was.
pub fn set_screen(config: &Path, s: Screen) -> Result<Map<String, Value>, String> {
    set_tui(config, "screen", s.name())
}

/// Writes `tui.<key>`, keeping every other key as it was.
fn set_tui(config: &Path, key: &str, value: &str) -> Result<Map<String, Value>, String> {
    let mut raw = krowk_harness::connect::read_config(config)?;
    let tui = raw.entry("tui").or_insert_with(|| Value::Object(Map::new()));
    let Some(tui) = tui.as_object_mut() else {
        return Err(format!("\"tui\" in {} is not an object — fix it by hand", config.display()));
    };
    tui.insert(key.into(), Value::String(value.into()));
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
        let written = set_screen(&config, Screen::Inline).unwrap();
        assert_eq!(Value::Object(written.clone()), json!({"tui": {"statusBar": false, "contentWidth": "full-width", "screen": "inline"}, "permissions": {"allow": ["Bash(ls)"], "defaultMode": "unhinged"}}));
        assert_eq!(from_config(&Value::Object(written)).0.screen, Screen::Inline, "read back as written");
        std::fs::write(&config, json!({"permissions": true, "tui": []}).to_string()).unwrap();
        assert!(set_content_width(&config, ContentWidth::Prose).is_err(), "a tui that is no object is not overwritten");
        assert!(set_screen(&config, Screen::Auto).is_err());
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
        assert_eq!(Settings::default().status_items, [Item::Model, Item::Device, Item::Tasks, Item::Subagents, Item::Help, Item::Branch, Item::Pr, Item::Cost], "the template's order");
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
        assert_eq!(ContentWidth::Prose.of(200), 80);
        assert_eq!(ContentWidth::Prose.of(40), 40, "a narrower terminal keeps all of its width");
        assert_eq!(ContentWidth::ProseWide.of(200), 120);
        assert_eq!(ContentWidth::ProseWide.of(100), 100);
        assert_eq!(ContentWidth::FullWidth.of(200), 200);
        assert_eq!(from_config(&json!({"tui": {"contentWidth": "prose-wide"}})).0.content_width, ContentWidth::ProseWide);
        assert_eq!(from_config(&json!({"tui": {"contentWidth": "full-width"}})).0.content_width, ContentWidth::FullWidth);
        let (s, w) = from_config(&json!({"tui": {"contentWidth": "wide"}}));
        assert_eq!(s.content_width, ContentWidth::Prose, "a malformed value leaves the default");
        assert!(w.len() == 1 && w[0].contains("\"wide\"") && w[0].contains("prose-wide") && w[0].contains("full-width"), "{w:?}");
        assert_eq!(ContentWidth::Prose.step(1), Some(ContentWidth::ProseWide));
        assert_eq!(ContentWidth::ProseWide.step(1), Some(ContentWidth::FullWidth));
        assert_eq!(ContentWidth::FullWidth.step(-1), Some(ContentWidth::ProseWide));
        assert_eq!(ContentWidth::FullWidth.step(1), None, "held →: nothing past full-width");
        assert_eq!(ContentWidth::Prose.step(-1), None);
    }

    #[test]
    fn the_screen_is_fullscreen_unless_inline_is_asked_for_or_zellij_runs_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default();
        assert_eq!(Settings::default().screen, Screen::Auto);
        assert!(Screen::Auto.fullscreen(&env(&[("TERM", "xterm-256color")])));
        assert!(!Screen::Auto.fullscreen(&env(&[("ZELLIJ", "0")])), "inline inside Zellij");
        assert!(Screen::Fullscreen.fullscreen(&env(&[("ZELLIJ", "0")])), "unless fullscreen is asked for");
        assert_eq!(from_config(&json!({"tui": {"screen": "fullscreen"}})).0.screen, Screen::Fullscreen);
        let (s, w) = from_config(&json!({"tui": {"screen": "inline"}}));
        assert!(s.screen == Screen::Inline && w.is_empty(), "{w:?}");
        let (s, w) = from_config(&json!({"tui": {"screen": "alt"}}));
        assert_eq!(s.screen, Screen::Auto, "a malformed value leaves the default");
        assert_eq!((Screen::Auto.step(1), Screen::Fullscreen.step(1), Screen::Inline.step(1)), (Some(Screen::Fullscreen), Some(Screen::Inline), None));
        assert_eq!((Screen::Inline.step(-1), Screen::Auto.step(-1)), (Some(Screen::Fullscreen), None));
        assert!(w.len() == 1 && w[0].contains("\"alt\"") && w[0].contains("inline"), "{w:?}");
    }

    #[test]
    fn r_tui_2_a_config_written_for_the_old_bar_still_reads() {
        let (s, w) = from_config(&json!({"tui": {"statusItems": ["model", "instance", "cost", "connectivity", "session", "todos", "subagents"]}}));
        assert!(w.is_empty(), "no warning for a name the bar once had: {w:?}");
        assert_eq!(s.status_items, [Item::Model, Item::Cost, Item::Tasks, Item::Subagents], "instance is model, todos is tasks, connectivity and session show nothing of their own");
    }
}
