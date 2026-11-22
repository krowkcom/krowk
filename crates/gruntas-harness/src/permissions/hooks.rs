//! The Claude-format `hooks` a settings file names (R-COMPAT-1), read with
//! the permission rules: a file whose hooks do not parse refuses the
//! prompt, and a repository's hooks widen what trusting it means. The
//! backend runs them itself.

use serde_json::Value;
use std::time::Duration;

/// The events gruntas runs hooks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    /// A subagent is done: Claude Code's `SubagentStop`, which blocks as
    /// `Stop` does — the subagent goes on.
    SubagentStop,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::Stop => "Stop",
            Event::SubagentStop => "SubagentStop",
        }
    }

    fn parse(s: &str) -> Option<Event> {
        Some(match s {
            "SessionStart" => Event::SessionStart,
            "UserPromptSubmit" => Event::UserPromptSubmit,
            "PreToolUse" => Event::PreToolUse,
            "PostToolUse" => Event::PostToolUse,
            "Stop" => Event::Stop,
            "SubagentStop" => Event::SubagentStop,
            _ => return None,
        })
    }
}

/// Claude Code's default: a minute.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest a hook may be given.
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Command {
    pub(crate) command: String,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Group {
    pub(crate) event: Event,
    pub(crate) matcher: Option<String>,
    pub(crate) commands: Vec<Command>,
    pub(crate) source: String,
}

/// Every hook that applies, in the order their files were read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hooks {
    pub(crate) groups: Vec<Group>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn extend(&mut self, other: Hooks) {
        self.groups.extend(other.groups);
    }
}

/// Reads a settings file's `hooks`. Events gruntas does not run
/// (`Notification`, `PreCompact`, …) and hook types other
/// than `command` are skipped: a file written for a newer Claude Code still
/// loads.
pub fn parse(v: &Value, source: &str) -> Result<Hooks, String> {
    let map = v.as_object().ok_or_else(|| format!("{source}: \"hooks\" must be an object of events"))?;
    let mut out = Hooks::default();
    for (name, groups) in map {
        let Some(event) = Event::parse(name) else { continue };
        let groups = groups.as_array().ok_or_else(|| format!("{source}: hooks.{name} must be a list"))?;
        for g in groups {
            let matcher = g.get("matcher").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty() && *m != "*").map(String::from);
            let list = g.get("hooks").and_then(Value::as_array).ok_or_else(|| format!("{source}: every hooks.{name} entry needs a \"hooks\" list"))?;
            let mut commands = Vec::new();
            for h in list {
                if h.get("type").and_then(Value::as_str).unwrap_or("command") != "command" {
                    continue;
                }
                let command = h.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()).ok_or_else(|| format!("{source}: a hooks.{name} command hook needs a \"command\""))?;
                let timeout = match h.get("timeout") {
                    None | Some(Value::Null) => DEFAULT_TIMEOUT,
                    Some(t) => match t.as_f64().filter(|t| t.is_finite() && *t > 0.0) {
                        // A day is more than any hook needs; what is longer
                        // is held to it rather than overflowing a clock.
                        Some(t) => Duration::from_secs_f64(t.min(MAX_TIMEOUT.as_secs_f64())),
                        None => return Err(format!("{source}: a hooks.{name} hook's \"timeout\" must be a positive number of seconds, not {t}")),
                    },
                };
                commands.push(Command { command: command.to_string(), timeout });
            }
            if !commands.is_empty() {
                out.groups.push(Group { event, matcher, commands, source: source.to_string() });
            }
        }
    }
    Ok(out)
}

