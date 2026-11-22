//! The model catalog, read from the models.dev cache krowk already keeps
//! for prices (R-PROV-2): a model's family, its context window and output
//! cap, whether it reasons and at which efforts, whether it calls tools, and
//! which wire API it is served on. Prices come from the same file through
//! the host's `Pricer`, which the CLI owns.
//!
//! The wire API is read the way models.dev records it — the AI SDK package
//! a provider (or one model of it) is served through, and a model's own
//! `shape` where the provider serves two:
//!
//! | models.dev | wire API |
//! |---|---|
//! | `shape: "responses"` | `openai-responses` |
//! | `shape: "completions"` | `chat-completions` |
//! | `@ai-sdk/openai` | `openai-responses` |
//! | `@ai-sdk/anthropic`, `…/anthropic` | `anthropic-messages` |
//! | `@ai-sdk/xai`, `@ai-sdk/openai-compatible`, `@openrouter/ai-sdk-provider` | `chat-completions` |
//! | anything else | none krowk speaks |

use crate::protocol::{Effort, WireApi};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::HashMap;

/// What the catalog knows of one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// e.g. `gpt-codex`, `grok`, `claude-opus`: picks the toolset preset.
    pub family: Option<String>,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub reasoning: bool,
    /// The efforts the model takes, on krowk's ladder. Empty for a model
    /// that reasons with no effort to choose.
    pub efforts: Vec<Effort>,
    pub tool_call: bool,
    /// The wire API it is served on, when it is one krowk speaks.
    pub wire_api: Option<WireApi>,
    /// Whether it reads images; none when the catalog does not say.
    pub images: Option<bool>,
}

#[derive(Deserialize)]
struct Provider<'a> {
    #[serde(default)]
    npm: Option<String>,
    #[serde(borrow, default)]
    models: Option<HashMap<String, &'a RawValue>>,
}

#[derive(Deserialize, Default)]
struct Model {
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    limit: Limit,
    #[serde(default)]
    provider: Option<Served>,
    #[serde(default)]
    modalities: Option<Modalities>,
}

#[derive(Deserialize, Default)]
struct ReasoningOption {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    values: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct Limit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Deserialize, Default)]
struct Served {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    shape: Option<String>,
}

/// The wire API an AI SDK package (and a model's `shape`) means.
pub fn wire_of(npm: Option<&str>, shape: Option<&str>) -> Option<WireApi> {
    match shape {
        Some("responses") => return Some(WireApi::OpenaiResponses),
        Some("completions") => return Some(WireApi::ChatCompletions),
        _ => {}
    }
    match npm? {
        "@ai-sdk/openai" => Some(WireApi::OpenaiResponses),
        p if p == "@ai-sdk/anthropic" || p.ends_with("/anthropic") => Some(WireApi::AnthropicMessages),
        "@ai-sdk/xai" | "@ai-sdk/openai-compatible" | "@openrouter/ai-sdk-provider" => Some(WireApi::ChatCompletions),
        _ => None,
    }
}

/// The model in a models.dev document: under its own provider first, else
/// the same id under any provider, so a router serving `gpt-5` finds
/// OpenAI's entry. Every field but the few above is skipped unread: the
/// file is megabytes.
///
/// The wire API is only ever the instance's own provider's word. How some
/// reseller serves the same id says nothing of how this instance's server
/// does — `openai/gpt-4o-mini` is on the Responses API at one gateway and on
/// Chat Completions at the next — so a borrowed entry lends its family,
/// limits and efforts, and no wire API.
pub fn lookup(raw: &[u8], provider: &str, model: &str) -> Option<ModelInfo> {
    let top: HashMap<String, &RawValue> = serde_json::from_slice(raw).ok()?;
    let of = |p: &RawValue| -> Option<ModelInfo> {
        let p: Provider = serde_json::from_str(p.get()).ok()?;
        let m: Model = serde_json::from_str(p.models?.get(model)?.get()).ok()?;
        let served = m.provider.unwrap_or_default();
        let efforts = m
            .reasoning_options
            .iter()
            .filter(|o| o.kind == "effort")
            .flat_map(|o| o.values.iter().filter_map(|v| v.as_str().and_then(Effort::parse)))
            .collect::<std::collections::BTreeSet<Effort>>()
            .into_iter()
            .collect();
        Some(ModelInfo {
            family: m.family.filter(|f| !f.is_empty()),
            context_window: m.limit.context.filter(|n| *n > 0),
            max_output: m.limit.output.filter(|n| *n > 0),
            reasoning: m.reasoning,
            efforts,
            tool_call: m.tool_call,
            wire_api: wire_of(served.npm.as_deref().or(p.npm.as_deref()), served.shape.as_deref()),
            images: m.modalities.map(|md| md.input.iter().any(|i| i == "image")),
        })
    };
    if let Some(info) = top.get(provider).and_then(|p| of(p)) {
        return Some(info);
    }
    // Any provider's entry, in a fixed order so the answer does not depend
    // on hash order.
    let mut names: Vec<&String> = top.keys().collect();
    names.sort();
    names.into_iter().find_map(|n| of(top[n])).map(|info| ModelInfo { wire_api: None, ..info })
}

/// One model a provider serves, as the catalog lists it: what choosing a
/// subagent's model needs (R-SUB-1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listed {
    pub id: String,
    pub family: String,
    /// `YYYY-MM-DD`, or empty when the catalog does not say.
    pub released: String,
    /// USD per million output tokens, when the catalog prices it.
    pub output_price: Option<f64>,
    /// It calls tools and reads and writes text only — not an image, audio
    /// or realtime model, which cannot run an agent's loop.
    pub agentic: bool,
}

#[derive(Deserialize, Default)]
struct ListedModel {
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    modalities: Option<Modalities>,
}

#[derive(Deserialize, Default)]
struct Cost {
    #[serde(default)]
    output: Option<f64>,
}

#[derive(Deserialize, Default)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

/// Every model the catalog lists under `provider`, sorted by id.
pub fn models(raw: &[u8], provider: &str) -> Vec<Listed> {
    let Ok(top) = serde_json::from_slice::<HashMap<String, &RawValue>>(raw) else { return Vec::new() };
    let Some(models) = top.get(provider).and_then(|p| serde_json::from_str::<Provider>(p.get()).ok()).and_then(|p| p.models) else { return Vec::new() };
    let mut out: Vec<Listed> = models
        .into_iter()
        .filter_map(|(id, m)| {
            let m: ListedModel = serde_json::from_str(m.get()).ok()?;
            let text_only = m.modalities.as_ref().is_none_or(|md| md.output.iter().all(|o| o == "text") && md.input.iter().any(|i| i == "text"));
            Some(Listed {
                agentic: m.tool_call && text_only,
                family: m.family.unwrap_or_default(),
                released: m.release_date.unwrap_or_default(),
                output_price: m.cost.and_then(|c| c.output),
                id,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The vendor a family belongs to: its first word (`claude` of
/// `claude-opus`, `gpt` of `gpt-mini`).
fn line_of(family: &str) -> &str {
    family.split('-').next().unwrap_or(family)
}

/// The cheaper tier below `model` (R-SUB-1's default for a subagent): of
/// the same provider's agentic models in the same vendor's line, the newest
/// that costs at most half as much per output token — Opus to Sonnet,
/// Sonnet to Haiku, a GPT to its mini. Between two of one release date, the
/// dearer, which is the nearer tier. None when the catalog does not price
/// `model` or lists nothing cheaper: the subagent then runs on `model`.
pub fn cheaper(listed: &[Listed], model: &str) -> Option<String> {
    let me = listed.iter().find(|m| m.id == model)?;
    let price = me.output_price.filter(|p| *p > 0.0)?;
    listed
        .iter()
        .filter(|m| m.agentic && m.id != me.id && line_of(&m.family) == line_of(&me.family) && m.output_price.is_some_and(|p| p <= price / 2.0))
        .max_by(|a, b| a.released.cmp(&b.released).then(a.output_price.partial_cmp(&b.output_price).unwrap_or(std::cmp::Ordering::Equal)).then(b.id.cmp(&a.id)))
        .map(|m| m.id.clone())
}

/// The newest agentic model of `family`: what an alias like Claude Code's
/// `haiku` (`claude-haiku`) names today.
pub fn newest(listed: &[Listed], family: &str) -> Option<String> {
    listed.iter().filter(|m| m.agentic && m.family == family).max_by(|a, b| a.released.cmp(&b.released).then(b.id.cmp(&a.id))).map(|m| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &[u8] = br#"{
        "openai": {"npm": "@ai-sdk/openai", "models": {
            "gpt-5.4": {"family": "gpt", "reasoning": true, "tool_call": true,
                "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}],
                "limit": {"context": 1050000, "output": 128000}, "cost": {"input": 2.5}},
            "gpt-4.1": {"family": "gpt", "reasoning": false, "tool_call": true, "limit": {"context": 1047576, "output": 32768}, "modalities": {"input": ["text", "image"], "output": ["text"]}},
            "gpt-text": {"family": "gpt", "tool_call": true, "modalities": {"input": ["text"], "output": ["text"]}}
        }},
        "xai": {"npm": "@ai-sdk/xai", "models": {
            "grok-4.7": {"family": "grok", "reasoning": true, "tool_call": true,
                "reasoning_options": [{"type": "effort", "values": ["low", "medium", "high", "xhigh"]}], "limit": {"context": 500000, "output": 500000}}
        }},
        "anthropic": {"npm": "@ai-sdk/anthropic", "models": {"claude-opus-5-5": {"family": "claude-opus", "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": ["low", "max", "banana"]}, {"type": "budget_tokens", "min": 1024}]}}},
        "neon": {"npm": "@ai-sdk/openai-compatible", "models": {"gpt-5-4-mini": {"family": "gpt-mini", "provider": {"npm": "@ai-sdk/openai", "shape": "responses"}}}},
        "router": {"npm": "@openrouter/ai-sdk-provider", "models": {"house": {"family": ""}}},
        "google": {"npm": "@ai-sdk/google", "models": {"gemini-3": {"family": "gemini"}}}
    }"#;

    #[test]
    fn r_prov_2_the_catalog_names_each_models_limits_efforts_and_wire_api() {
        let g = lookup(DOC, "openai", "gpt-5.4").unwrap();
        assert_eq!(g.family.as_deref(), Some("gpt"));
        assert_eq!((g.context_window, g.max_output, g.reasoning, g.tool_call), (Some(1_050_000), Some(128_000), true, true));
        assert_eq!(g.efforts, vec![Effort::None, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh]);
        assert_eq!(g.wire_api, Some(WireApi::OpenaiResponses));
        assert!(!lookup(DOC, "openai", "gpt-4.1").unwrap().reasoning);
        assert_eq!(lookup(DOC, "xai", "grok-4.7").unwrap().wire_api, Some(WireApi::ChatCompletions));
        let c = lookup(DOC, "anthropic", "claude-opus-5-5").unwrap();
        assert_eq!((c.wire_api, c.efforts.clone()), (Some(WireApi::AnthropicMessages), vec![Effort::Low, Effort::Max]), "an unknown value is skipped, the rest kept in ladder order");
        // A model's own package and shape outrank its provider's.
        assert_eq!(lookup(DOC, "neon", "gpt-5-4-mini").unwrap().wire_api, Some(WireApi::OpenaiResponses));
        assert_eq!(lookup(DOC, "router", "house").unwrap().family, None, "an empty family is none");
        assert_eq!(lookup(DOC, "google", "gemini-3").unwrap().wire_api, None, "a wire API krowk does not speak");
        // The same id under any provider, when its own does not list it —
        // its family and efforts, never its wire API.
        let borrowed = lookup(DOC, "openrouter", "grok-4.7").unwrap();
        assert_eq!((borrowed.family.as_deref(), borrowed.efforts.len(), borrowed.wire_api), (Some("grok"), 4, None));
        let reseller = lookup(DOC, "some-gateway", "gpt-5-4-mini").unwrap();
        assert_eq!((reseller.family.as_deref(), reseller.wire_api), (Some("gpt-mini"), None), "neon's Responses shape is neon's, not this gateway's");
        assert_eq!(lookup(DOC, "openai", "nope"), None);
        assert_eq!(lookup(b"not json", "openai", "gpt-5.4"), None);
        let images = |m| lookup(DOC, "openai", m).unwrap().images;
        assert_eq!((images("gpt-4.1"), images("gpt-text"), images("gpt-5.4")), (Some(true), Some(false), None), "an image input, none, and a catalog that does not say");
        assert_eq!(wire_of(Some("@ai-sdk/google-vertex/anthropic"), None), Some(WireApi::AnthropicMessages));
        assert_eq!(wire_of(Some("@ai-sdk/openai"), Some("completions")), Some(WireApi::ChatCompletions));
    }

    #[test]
    fn r_sub_1_a_subagents_default_model_is_the_cheaper_tier_the_catalog_lists() {
        let doc = br#"{"anthropic": {"models": {
            "claude-opus-5-5": {"family": "claude-opus", "tool_call": true, "release_date": "2026-09-22", "cost": {"output": 20}},
            "claude-sonnet-5": {"family": "claude-sonnet", "tool_call": true, "release_date": "2026-06-29", "cost": {"output": 10}},
            "claude-haiku-4-5": {"family": "claude-haiku", "tool_call": true, "release_date": "2025-10-15", "cost": {"output": 5}},
            "claude-haiku-4-5-20251001": {"family": "claude-haiku", "tool_call": true, "release_date": "2025-10-15", "cost": {"output": 5}},
            "claude-3-haiku": {"family": "claude-haiku", "tool_call": true, "release_date": "2024-03-07", "cost": {"output": 1.25}}
        }}, "openai": {"models": {
            "gpt-6-sol": {"family": "gpt-sol", "tool_call": true, "release_date": "2026-09-22", "cost": {"output": 30}},
            "gpt-6-luna": {"family": "gpt-luna", "tool_call": true, "release_date": "2026-09-22", "cost": {"output": 0.5}},
            "gpt-6-mini": {"family": "gpt-mini", "tool_call": true, "release_date": "2026-09-22", "cost": {"output": 4}},
            "gpt-realtime-3": {"family": "gpt", "tool_call": true, "release_date": "2026-09-23", "cost": {"output": 2}, "modalities": {"input": ["text", "audio"], "output": ["text", "audio"]}},
            "o9": {"family": "o", "tool_call": true, "release_date": "2026-09-24", "cost": {"output": 1}}
        }}}"#;
        let anthropic = models(doc, "anthropic");
        assert_eq!(anthropic.len(), 5);
        assert_eq!(cheaper(&anthropic, "claude-opus-5-5").as_deref(), Some("claude-sonnet-5"), "one tier down, not the cheapest there is");
        assert_eq!(cheaper(&anthropic, "claude-sonnet-5").as_deref(), Some("claude-haiku-4-5"), "the newest at half the price; of one date the first id");
        assert_eq!(cheaper(&anthropic, "claude-3-haiku"), None, "nothing cheaper: the subagent runs on the parent's model");
        assert_eq!(cheaper(&anthropic, "claude-unknown"), None);
        let openai = models(doc, "openai");
        assert_eq!(cheaper(&openai, "gpt-6-sol").as_deref(), Some("gpt-6-mini"), "of one date the dearer; never a realtime model or another vendor's line");
        assert_eq!(newest(&anthropic, "claude-haiku").as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(newest(&anthropic, "claude-fable"), None);
        assert!(models(b"nope", "openai").is_empty() && models(doc, "xai").is_empty());
    }
}
