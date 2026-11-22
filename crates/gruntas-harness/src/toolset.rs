//! Toolset presets: which edit tool a model is offered (R-TOOL-2).
//!
//! Every model gets the same core — read, write, bash, grep, glob — and one
//! edit tool in the format it was trained on, because a model edits
//! measurably better in its own format than in anyone else's:
//!
//! | preset   | edit tool        | chosen for the families |
//! |----------|------------------|-------------------------|
//! | `claude` | `str_replace`    | `claude-*`              |
//! | `gpt`    | `apply_patch`    | `gpt-*`, `o*`, `codex`  |
//! | `grok`   | `search_replace` | `grok-*`                |
//!
//! The model's family picks the preset: the models.dev catalog's `family`
//! field when the catalog knows the model, else the family read off the
//! model id, so a router's `openai/gpt-5` is a GPT all the same. Config's
//! `toolset` overrides that for every model, and `--toolset` overrides both
//! for one prompt. A family no preset claims gets `claude`: `str_replace`
//! is a plain JSON tool any model that calls tools can drive.
//!
//! The preset is chosen per turn, like the model: a turn's tools are
//! whatever its model is best at, and the context record says which.

/// The edit tool a preset offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTool {
    /// Claude's: an exact, unique `old_str` replaced by `new_str`.
    StrReplace,
    /// GPT's and Codex's: a V4A patch envelope, freeform where the wire API
    /// takes grammar tools and a JSON string elsewhere.
    ApplyPatch,
    /// Grok's: `old_string` replaced by `new_string`, under the names its
    /// tools use.
    SearchReplace,
}

/// One entry of the registry.
#[derive(Debug, PartialEq, Eq)]
pub struct Preset {
    /// What `--toolset` and config's `toolset` call it.
    pub name: &'static str,
    pub edit: EditTool,
    /// The catalog families it is chosen for: a family matches when it is
    /// one of these or starts with one and a `-` (`gpt-codex` is a `gpt`).
    pub families: &'static [&'static str],
}

/// The registry, in the order `--toolset` lists them.
pub const PRESETS: &[Preset] = &[
    Preset { name: "claude", edit: EditTool::StrReplace, families: &["claude"] },
    Preset { name: "gpt", edit: EditTool::ApplyPatch, families: &["gpt", "o", "codex"] },
    Preset { name: "grok", edit: EditTool::SearchReplace, families: &["grok"] },
];

/// What a family no preset claims gets.
pub const FALLBACK: &Preset = &PRESETS[0];

/// The preset of this name.
pub fn by_name(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// The names, for an error that lists them.
pub fn names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

/// The preset a family claims, if any does.
pub fn for_family(family: &str) -> Option<&'static Preset> {
    let family = family.trim().to_ascii_lowercase();
    PRESETS.iter().find(|p| p.families.iter().any(|f| family == *f || family.strip_prefix(f).is_some_and(|rest| rest.starts_with('-'))))
}

/// The family a model id names, for a model the catalog does not know: the
/// first part of the id (split at `/`, `.`, `:` and `@`, so a router's or a
/// cloud's prefix falls away) that starts with a family's name. OpenAI's
/// reasoning models are `o` followed by a digit.
pub fn family_from_id(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    for part in model.split(['/', '.', ':', '@']) {
        for f in ["claude", "gpt", "codex", "grok"] {
            if part.starts_with(f) {
                return Some(f);
            }
        }
        let mut c = part.chars();
        if c.next() == Some('o') && c.next().is_some_and(|d| d.is_ascii_digit()) {
            return Some("o");
        }
    }
    None
}

/// The major version a `gpt-N…` part of a model id names: 5 for `gpt-5.4`,
/// `openai/gpt-5-mini` or `gpt-5o`.
fn gpt_major(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    model.split(['/', ':', '@']).find_map(|part| {
        let rest = part.strip_prefix("gpt-")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// A model OpenAI trained on freeform (custom, grammar) tools: GPT-5 and
/// every GPT after it, and the Codex models. Only these are offered
/// `apply_patch` as a freeform tool; the rest get its JSON form.
pub fn takes_custom_tools(model: &str) -> bool {
    family_from_id(model) == Some("codex") || model.to_ascii_lowercase().contains("codex") || gpt_major(model).is_some_and(|n| n >= 5)
}

/// Whether a model the catalog does not know reasons, read off its id:
/// the o-series, the Codex models and GPT-5 on. It decides whether an
/// OpenAI request asks for encrypted reasoning back, which a model that
/// does not reason refuses.
pub fn reasons(model: &str) -> bool {
    matches!(family_from_id(model), Some("o")) || takes_custom_tools(model)
}

/// Where a turn's preset came from, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--toolset`, or the protocol's `toolset` on the prompt.
    Prompt,
    /// `toolset` in config.json.
    Config,
    /// The catalog's family for the model.
    Catalog,
    /// The family read off the model id.
    ModelId,
    /// No family any preset claims.
    Fallback,
}

/// Picks a turn's preset. An override that names no preset is an error,
/// never a silent fallback: somebody asked for it by name.
pub fn choose(prompt: Option<&str>, config: Option<&str>, catalog_family: Option<&str>, model: &str) -> Result<(&'static Preset, Source), String> {
    let named = |n: &str, what: &str| by_name(n.trim()).ok_or_else(|| format!("{what} {n:?} is not a toolset — one of {}", names().join(", ")));
    if let Some(n) = prompt.filter(|n| !n.trim().is_empty()) {
        return Ok((named(n, "--toolset")?, Source::Prompt));
    }
    if let Some(n) = config.filter(|n| !n.trim().is_empty()) {
        return Ok((named(n, "config toolset")?, Source::Config));
    }
    if let Some(p) = catalog_family.and_then(for_family) {
        return Ok((p, Source::Catalog));
    }
    if let Some(p) = family_from_id(model).and_then(for_family) {
        return Ok((p, Source::ModelId));
    }
    Ok((FALLBACK, Source::Fallback))
}

/// A turn's tools: the preset, and whether its model takes freeform
/// (grammar) tools, which decides how `apply_patch` is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toolset {
    pub preset: &'static Preset,
    pub custom_tools: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_tool_2_the_family_picks_the_edit_tool_and_an_override_wins() {
        let edit = |prompt, config, family, model| choose(prompt, config, family, model).unwrap();
        // The catalog's families, as models.dev names them.
        for (family, preset) in [
            ("claude-opus", "claude"),
            ("claude-sonnet", "claude"),
            ("gpt", "gpt"),
            ("gpt-codex", "gpt"),
            ("gpt-mini", "gpt"),
            ("o", "gpt"),
            ("o-mini", "gpt"),
            ("grok", "grok"),
            ("grok-build", "grok"),
        ] {
            assert_eq!(edit(None, None, Some(family), "x").0.name, preset, "{family}");
            assert_eq!(edit(None, None, Some(family), "x").1, Source::Catalog);
        }
        // "gptx" is not the gpt family, and "opus" is not "o".
        assert_eq!(for_family("gptx"), None);
        assert_eq!(for_family("opus"), None);
        // Ids the catalog does not know, read for their family.
        for (model, preset) in [
            ("claude-opus-5-5", "claude"),
            ("us.anthropic.claude-haiku-4-5-20251001-v1:0", "claude"),
            ("gpt-5.1-codex", "gpt"),
            ("openai/gpt-5", "gpt"),
            ("o3-mini", "gpt"),
            ("grok-4-fast", "grok"),
            ("x-ai/grok-code-fast-1", "grok"),
        ] {
            assert_eq!(edit(None, None, None, model), (by_name(preset).unwrap(), Source::ModelId), "{model}");
        }
        assert_eq!(edit(None, None, None, "llama-4"), (FALLBACK, Source::Fallback));
        assert_eq!(edit(None, None, None, "ollama/opus-clone").1, Source::Fallback, "no family reads out of a word that merely starts with o");
        // The catalog outranks the id; config outranks the catalog; the
        // prompt outranks everything.
        assert_eq!(edit(None, None, Some("grok"), "gpt-5").0.name, "grok");
        assert_eq!(edit(None, Some("gpt"), Some("claude-opus"), "claude-opus-5-5"), (by_name("gpt").unwrap(), Source::Config));
        assert_eq!(edit(Some("grok"), Some("gpt"), Some("claude-opus"), "claude-opus-5-5"), (by_name("grok").unwrap(), Source::Prompt));
        assert!(choose(Some("vim"), None, None, "gpt-5").unwrap_err().contains("claude, gpt, grok"));
        assert!(choose(None, Some("vim"), None, "gpt-5").unwrap_err().contains("config toolset"));
    }

    #[test]
    fn r_prov_1_freeform_tools_and_reasoning_are_read_off_openai_model_ids() {
        for m in ["gpt-5", "gpt-5.4", "openai/gpt-5-mini", "gpt-6-sol", "gpt-5.3-codex", "codex-mini-latest"] {
            assert!(takes_custom_tools(m), "{m}");
            assert!(reasons(m), "{m}");
        }
        for m in ["gpt-4.1", "gpt-4o", "o3", "claude-opus-5-5", "grok-4.7"] {
            assert!(!takes_custom_tools(m), "{m}");
        }
        assert!(reasons("o3") && reasons("o4-mini"));
        assert!(!reasons("gpt-4.1") && !reasons("grok-4.7"));
    }
}
