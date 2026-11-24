//! The rollover ladder for launched sessions (Rollover for launched
//! sessions D1, D2; R-RL-1, R-RL-2): where a session the Launcher started
//! goes when its account meets rollover's early-switch rule or hits its
//! limit. The rungs, tried in order: another subscription of the same
//! harness on the same model; a subscription of the other harness at the
//! equivalent tier; API-key accounts, the same harness first, then the
//! other, while the spend cap allows. Within a rung, rollover's headroom
//! ranking (`usage::Data::rank`) orders the accounts.
//!
//! A session keeps its tier on every rung: a model the tier table does not
//! place never crosses harnesses, and nothing ever drops a tier. The table
//! and the rungs are config.json's `ladder`; the defaults stand without it.
//!
//! The API-key rungs spend money, so a session climbs onto them within a
//! spend cap (D3; R-RL-3): `keyCapUsd`, $10 unless set, counted from its
//! spend as it entered the key rung (`KeySpend`). At the cap it stops,
//! input-required, with `KeySpend::capped`'s reason; `keys_open` is what
//! `steps` is told. Attended Codex has no spend reading, so it never climbs
//! onto a key rung.
//!
//! Pure: it reads the registry and the usage records it is given, and
//! decides nothing about whether an account is signed in — the caller
//! passes over one that is not, as `--account auto` does.

use crate::instances::Registry;
use crate::launch::Harness;
use crate::permissions::rules::wildcard;
use crate::protocol::ModelRef;
use crate::usage::Data;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A model tier (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    Frontier,
    Standard,
    Fast,
}

/// A rung of the ladder (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Rung {
    /// Another subscription account of the same harness, the same model.
    SameHarness,
    /// A subscription account of the other harness, at the equivalent tier.
    OtherHarness,
    /// API-key accounts, the same harness first, within the spend cap.
    ApiKey,
}

/// One tier's model ids on each harness: `*` matches any run of
/// characters, case aside. The first id without a `*` is the model a
/// session moving to that harness at this tier runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TierModels {
    pub tier: Tier,
    #[serde(default)]
    pub claude: Vec<String>,
    #[serde(default)]
    pub codex: Vec<String>,
}

/// config.json's `ladder`: the tier table and the rungs in the order they
/// are tried; a rung left out is never tried. Either absent → its default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ladder {
    #[serde(default = "default_tiers")]
    pub tiers: Vec<TierModels>,
    #[serde(default = "default_rungs")]
    pub rungs: Vec<Rung>,
    /// The most a session spends on API-key rungs, in US dollars, from
    /// when it entered one (D3).
    #[serde(default = "default_key_cap")]
    pub key_cap_usd: f64,
}

impl Default for Ladder {
    fn default() -> Ladder {
        Ladder { tiers: default_tiers(), rungs: default_rungs(), key_cap_usd: default_key_cap() }
    }
}

/// D3's default cap.
fn default_key_cap() -> f64 {
    10.0
}

fn default_rungs() -> Vec<Rung> {
    vec![Rung::SameHarness, Rung::OtherHarness, Rung::ApiKey]
}

/// D2's defaults, each with its current ids.
fn default_tiers() -> Vec<TierModels> {
    let t = |tier, claude: &[&str], codex: &[&str]| TierModels { tier, claude: claude.iter().map(|s| s.to_string()).collect(), codex: codex.iter().map(|s| s.to_string()).collect() };
    vec![
        t(Tier::Frontier, &["opus", "claude-opus-*", "opus[*"], &["gpt-6-astra", "gpt-5.6-sol", "gpt-5.5"]),
        t(Tier::Standard, &["sonnet", "claude-sonnet-*", "sonnet[*"], &["gpt-5.6-terra", "gpt-5.4"]),
        t(Tier::Fast, &["haiku", "claude-haiku-*"], &["gpt-5.6-luna", "gpt-5.4-mini"]),
    ]
}

/// Where the ladder would move a session: the account, at the model it
/// runs there, and the rung it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub rung: Rung,
    pub to: ModelRef,
}

impl Ladder {
    /// Whether this is the default ladder, which config.json leaves out.
    pub fn is_default(&self) -> bool {
        *self == Ladder::default()
    }

    /// The tier of `model` on `harness`: the first tier one of whose ids
    /// matches it. None → the model is in no tier.
    pub fn tier_of(&self, harness: Harness, model: &str) -> Option<Tier> {
        let model = model.to_ascii_lowercase();
        self.tiers.iter().find(|t| ids(t, harness).iter().any(|p| wildcard(&p.to_ascii_lowercase(), &model))).map(|t| t.tier)
    }

    /// The model a session at `tier` runs on `harness`; none when the
    /// table names no plain id there.
    pub fn model_at(&self, harness: Harness, tier: Tier) -> Option<&str> {
        self.tiers.iter().filter(|t| t.tier == tier).flat_map(|t| ids(t, harness)).find(|id| !id.contains('*')).map(String::as_str)
    }

    /// Every account a session on `from` (its account, and the model the
    /// launcher passed, else the account's default) could move to, best
    /// first: the rungs in order, each ranked by headroom. `keys` is the
    /// spend cap's say: false once the session has spent it, or when its
    /// spend cannot be read, and the API-key rung is passed over. Empty →
    /// none left.
    pub fn steps(&self, registry: &Registry, data: &Data, from: &ModelRef, keys: bool, now_ms: i64) -> Vec<Step> {
        // A session logged under a name its account has since lost.
        let from_name = registry.current(&from.instance);
        let Some(own) = registry.instances.get(&from_name).and_then(|i| harness_of(i.kind)) else { return Vec::new() };
        let other = match own {
            Harness::Claude => Harness::Codex,
            Harness::Codex => Harness::Claude,
        };
        // The other harness's model at this session's tier; none → no rung
        // crosses, as nothing there is known to be the same tier.
        let across = self.tier_of(own, &from.model).and_then(|t| self.model_at(other, t));
        let login = data.login_of(&from_name);
        // An account that listed its models has this one when it lists it,
        // or on Claude Code an alias of its tier: it lists `opus`, not
        // `claude-opus-5-5`. Codex's ids of one tier are different models.
        let rank = |rung, harness: Harness, keyed: bool, model: Option<&str>| -> Vec<Step> {
            let Some(model) = model else { return Vec::new() };
            let tier = self.tier_of(harness, model);
            let candidates: Vec<ModelRef> = registry
                .instances
                .iter()
                .filter(|(n, i)| i.kind == harness.kind() && i.backend.is_some() && i.api_key_env.is_empty() != keyed && **n != from_name)
                .filter(|(n, _)| data.instance(n).is_none_or(|r| r.models.is_empty() || r.models.iter().any(|m| m == model || (alias(harness, m) && self.tier_of(harness, m).is_some_and(|t| Some(t) == tier)))))
                .map(|(n, _)| ModelRef { instance: n.clone(), model: model.to_string() })
                .collect();
            data.rank(login, &candidates, now_ms).into_iter().map(|r| Step { rung, to: r.model }).collect()
        };
        let mut out = Vec::new();
        for (i, rung) in self.rungs.iter().enumerate() {
            if self.rungs[..i].contains(rung) {
                continue;
            }
            match rung {
                Rung::SameHarness => out.extend(rank(*rung, own, false, Some(&from.model))),
                Rung::OtherHarness => out.extend(rank(*rung, other, false, across)),
                Rung::ApiKey if keys => {
                    out.extend(rank(*rung, own, true, Some(&from.model)));
                    out.extend(rank(*rung, other, true, across));
                }
                Rung::ApiKey => {}
            }
        }
        out
    }

    /// The ladder's pick: the first of `steps`, none when none is left.
    pub fn next(&self, registry: &Registry, data: &Data, from: &ModelRef, keys: bool, now_ms: i64) -> Option<Step> {
        self.steps(registry, data, from, keys, now_ms).into_iter().next()
    }
}

/// A session on an API-key rung: the account it entered on, and its spend
/// reading then, from which the cap counts (D3). The spend reading is the
/// mod's session cost (Claude Code) or the Codex engine's usage
/// (unattended Codex), in US dollars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeySpend {
    pub account: String,
    pub baseline_usd: f64,
    pub cap_usd: f64,
}

impl KeySpend {
    /// A session entering the key rung on `step`'s account, its spend
    /// reading `reading_usd` as it does.
    pub fn enter(step: &Step, reading_usd: f64, cap_usd: f64) -> KeySpend {
        KeySpend { account: step.to.instance.clone(), baseline_usd: reading_usd, cap_usd }
    }

    /// What it has spent on the key rung, at spend reading `reading_usd`.
    pub fn spent(&self, reading_usd: f64) -> f64 {
        (reading_usd - self.baseline_usd).max(0.0)
    }

    /// Why the session stops, once its spend since the baseline reached
    /// the cap; none while it is under.
    pub fn capped(&self, reading_usd: f64) -> Option<String> {
        let spent = self.spent(reading_usd);
        (spent >= self.cap_usd).then(|| format!("Stopped at the API-key spend cap: ${spent:.2} spent on {} since it moved onto the API-key rung, cap ${:.2} (ladder.keyCapUsd in ~/.gruntas/config.json). Raise the cap, or continue once a subscription has room.", self.account, self.cap_usd))
    }
}

/// Whether the API-key rung is open to a session (`steps`' `keys`): never
/// for attended Codex, which has no spend reading, nor for any session
/// whose spend cannot be read (`reading_usd` none); on the key rung
/// already (`on_key`), while it is under the cap.
pub fn keys_open(harness: Harness, attended: bool, reading_usd: Option<f64>, on_key: Option<&KeySpend>) -> bool {
    if attended && harness == Harness::Codex {
        return false;
    }
    match (reading_usd, on_key) {
        (None, _) => false,
        (Some(r), Some(k)) => k.capped(r).is_none(),
        (Some(_), None) => true,
    }
}

/// The switch notice for `step`: the account, its model, its rung, and on
/// the API-key rung the cap (D3).
pub fn notice(step: &Step, cap_usd: f64) -> String {
    let (account, model) = (&step.to.instance, &step.to.model);
    match step.rung {
        Rung::SameHarness => format!("Continuing on {account} ({model}): another subscription of the same harness."),
        Rung::OtherHarness => format!("Continuing on {account} ({model}): a subscription of the other harness."),
        Rung::ApiKey => format!("Continuing on {account} ({model}): an API-key account, billed per use, up to ${cap_usd:.2} this session."),
    }
}

fn ids(t: &TierModels, harness: Harness) -> &[String] {
    match harness {
        Harness::Claude => &t.claude,
        Harness::Codex => &t.codex,
    }
}

/// A Claude Code alias (`opus`, `sonnet[1m]`): an id without a dash.
fn alias(harness: Harness, id: &str) -> bool {
    harness == Harness::Claude && !id.contains('-')
}

fn harness_of(kind: &str) -> Option<Harness> {
    [Harness::Claude, Harness::Codex].into_iter().find(|h| h.kind() == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::from_config_json;
    use crate::protocol::LimitState;
    use crate::usage::{InstanceRecord, Window};
    use serde_json::json;

    /// Two subscriptions and one key account of each harness, and an
    /// Anthropic API instance that is no harness account.
    fn registry(ladder: serde_json::Value) -> Registry {
        let mut raw = json!({"instances": {
            "claude:b": {"kind": "claude-code", "configDir": "/h/b"},
            "claude:key": {"kind": "claude-code", "configDir": "/h/k", "apiKeyEnv": "ANTHROPIC_API_KEY"},
            "codex:t": {"kind": "codex-app-server", "codexHome": "/h/t"},
            "codex:key": {"kind": "codex-app-server", "codexHome": "/h/ck", "apiKeyEnv": "OPENAI_API_KEY"},
        }});
        if !ladder.is_null() {
            raw["ladder"] = ladder;
        }
        Registry::resolve(&from_config_json(&raw).unwrap(), &|k| if k == "HOME" { "/h".into() } else { String::new() })
    }

    /// Each instance its own login at `used` percent of its five-hour
    /// window; `None` limited.
    fn usage(rows: &[(&str, Option<f64>)]) -> Data {
        let mut d = Data::default();
        for (name, used) in rows {
            let status = if used.is_some() { LimitState::Allowed } else { LimitState::Limited };
            d.logins.entry(name.to_string()).or_default().insert("five_hour".into(), Window { status, used_percent: Some(used.unwrap_or(100.0)), resets_at_ms: None, seen_at_ms: 1_000, first_span_ms: None });
            d.instances.insert(name.to_string(), InstanceRecord { login: name.to_string(), models: Vec::new(), seen_at_ms: 1_000, config_home: None });
        }
        d
    }

    fn on(instance: &str, model: &str) -> ModelRef {
        ModelRef { instance: instance.into(), model: model.into() }
    }

    fn steps(reg: &Registry, d: &Data, from: ModelRef, keys: bool) -> Vec<(Rung, String, String)> {
        reg.ladder.steps(reg, d, &from, keys, 2_000).into_iter().map(|s| (s.rung, s.to.instance, s.to.model)).collect()
    }

    fn row(rung: Rung, instance: &str, model: &str) -> (Rung, String, String) {
        (rung, instance.into(), model.into())
    }

    #[test]
    fn every_rung_in_order_at_the_sessions_tier() {
        let reg = registry(json!(null));
        let d = usage(&[("claude", Some(99.0)), ("claude:b", Some(20.0)), ("codex", Some(50.0)), ("codex:t", Some(10.0))]);
        // Same harness and model, then Codex at Opus's tier by headroom
        // (codex:t has more left), then keys: Claude Code's before Codex's.
        assert_eq!(
            steps(&reg, &d, on("claude", "claude-opus-5-5"), true),
            [row(Rung::SameHarness, "claude:b", "claude-opus-5-5"), row(Rung::OtherHarness, "codex:t", "gpt-6-astra"), row(Rung::OtherHarness, "codex", "gpt-6-astra"), row(Rung::ApiKey, "claude:key", "claude-opus-5-5"), row(Rung::ApiKey, "codex:key", "gpt-6-astra")]
        );
        // From Codex's mid model: Claude Code at Sonnet.
        assert_eq!(steps(&reg, &d, on("codex:t", "gpt-5.4"), false), [row(Rung::SameHarness, "codex", "gpt-5.4"), row(Rung::OtherHarness, "claude:b", "sonnet")]);
        // The pick is the first; a limited same-harness account is passed over.
        let d = usage(&[("claude", Some(99.0)), ("claude:b", None), ("codex", None), ("codex:t", Some(10.0))]);
        assert_eq!(reg.ladder.next(&reg, &d, &on("claude", "opus"), true, 2_000).map(|s| (s.rung, s.to.instance)), Some((Rung::OtherHarness, "codex:t".into())));
    }

    #[test]
    fn the_cap_closes_the_key_rung_and_nothing_left_is_none() {
        let reg = registry(json!(null));
        let spent = usage(&[("claude", None), ("claude:b", None), ("codex", None), ("codex:t", Some(95.0))]);
        assert_eq!(steps(&reg, &spent, on("claude", "claude-haiku-4-5"), true), [row(Rung::ApiKey, "claude:key", "claude-haiku-4-5"), row(Rung::ApiKey, "codex:key", "gpt-5.6-luna")]);
        assert_eq!(reg.ladder.next(&reg, &spent, &on("claude", "claude-haiku-4-5"), false, 2_000), None, "at the cap, no subscription left: none");
        // An account that is no harness account has no ladder.
        assert!(steps(&reg, &spent, on("anthropic", "claude-opus-5-5"), true).is_empty());
    }

    #[test]
    fn a_setting_reorders_and_drops_rungs() {
        let d = usage(&[("claude", Some(99.0)), ("claude:b", Some(20.0)), ("codex", Some(50.0)), ("codex:t", Some(10.0))]);
        let reg = registry(json!({"rungs": ["api-key", "same-harness", "api-key"]}));
        assert_eq!(steps(&reg, &d, on("claude", "opus"), true), [row(Rung::ApiKey, "claude:key", "opus"), row(Rung::ApiKey, "codex:key", "gpt-6-astra"), row(Rung::SameHarness, "claude:b", "opus")]);
        let reg = registry(json!({"rungs": ["other-harness"]}));
        assert_eq!(steps(&reg, &d, on("claude", "opus"), true), [row(Rung::OtherHarness, "codex:t", "gpt-6-astra"), row(Rung::OtherHarness, "codex", "gpt-6-astra")]);
    }

    #[test]
    fn tiers_match_by_pattern_and_a_session_never_drops_one() {
        let l = Ladder::default();
        for (h, model, tier) in [(Harness::Claude, "opus", Tier::Frontier), (Harness::Claude, "Claude-Opus-5-5", Tier::Frontier), (Harness::Claude, "opus[1m]", Tier::Frontier), (Harness::Claude, "claude-sonnet-5", Tier::Standard), (Harness::Claude, "haiku", Tier::Fast), (Harness::Codex, "gpt-6-astra", Tier::Frontier), (Harness::Codex, "gpt-5.5", Tier::Frontier), (Harness::Codex, "gpt-5.6-terra", Tier::Standard), (Harness::Codex, "gpt-5.6-luna", Tier::Fast), (Harness::Codex, "gpt-5.4-mini", Tier::Fast)] {
            assert_eq!(l.tier_of(h, model), Some(tier), "{model}");
        }
        assert_eq!(l.tier_of(Harness::Claude, "gpt-6-astra"), None, "another harness's id is no tier here");
        assert_eq!((l.model_at(Harness::Claude, Tier::Frontier), l.model_at(Harness::Codex, Tier::Fast)), (Some("opus"), Some("gpt-5.6-luna")));
        // A model in no tier stays on its own harness; a tier the other
        // harness has no model for is never stepped down to another.
        let d = usage(&[("claude", Some(99.0)), ("claude:b", Some(20.0)), ("codex", Some(50.0))]);
        let reg = registry(json!(null));
        assert_eq!(steps(&reg, &d, on("claude", "some-new-model"), true), [row(Rung::SameHarness, "claude:b", "some-new-model"), row(Rung::ApiKey, "claude:key", "some-new-model")]);
        let reg = registry(json!({"tiers": [{"tier": "frontier", "claude": ["opus"]}, {"tier": "standard", "claude": ["sonnet"], "codex": ["gpt-5.4"]}]}));
        assert_eq!(steps(&reg, &d, on("claude", "opus"), false), [row(Rung::SameHarness, "claude:b", "opus")]);
        assert_eq!(steps(&reg, &d, on("claude", "sonnet"), false), [row(Rung::SameHarness, "claude:b", "sonnet"), row(Rung::OtherHarness, "codex", "gpt-5.4"), row(Rung::OtherHarness, "codex:t", "gpt-5.4")]);
    }

    #[test]
    fn an_account_without_the_model_or_on_the_same_login_is_no_step() {
        let reg = registry(json!(null));
        let mut d = usage(&[("claude", Some(99.0)), ("claude:b", Some(20.0)), ("codex", Some(50.0))]);
        d.instances.get_mut("codex").unwrap().models = vec!["gpt-5.4".into()];
        d.instances.get_mut("claude:b").unwrap().login = "claude".into();
        assert_eq!(steps(&reg, &d, on("claude", "opus"), false), [row(Rung::OtherHarness, "codex:t", "gpt-6-astra")]);
    }

    #[test]
    fn claude_codes_aliases_cover_the_full_ids_of_their_tier_and_a_renamed_account_climbs() {
        let mut reg = registry(json!(null));
        let mut d = usage(&[("claude", Some(99.0)), ("claude:b", Some(20.0)), ("claude:c", Some(30.0)), ("codex", None), ("codex:t", None)]);
        // As Claude Code lists its models: aliases, not the account default's id.
        d.instances.get_mut("claude:b").unwrap().models = ["default", "opus", "claude-fable-5-1[1m]", "sonnet", "haiku"].map(String::from).to_vec();
        d.instances.get_mut("claude:c").unwrap().models = vec!["sonnet".into(), "claude-opus-4-1".into()];
        let c = reg.instances["claude:b"].clone();
        reg.instances.insert("claude:c".into(), c);
        assert_eq!(steps(&reg, &d, on("claude", "claude-opus-5-5"), false), [row(Rung::SameHarness, "claude:b", "claude-opus-5-5")]);
        // Codex's ids of one tier are different models: gpt-5.5 is no gpt-6-astra.
        let mut d2 = usage(&[("claude", Some(99.0)), ("codex", Some(10.0))]);
        d2.instances.get_mut("codex").unwrap().models = vec!["gpt-5.5".into()];
        assert!(!steps(&reg, &d2, on("claude", "opus"), false).iter().any(|s| s.1 == "codex"));
        // A session logged under its account's old name.
        reg.renamed.insert("claude:old".into(), "claude".into());
        assert_eq!(steps(&reg, &d, on("claude:old", "claude-opus-5-5"), false), [row(Rung::SameHarness, "claude:b", "claude-opus-5-5")]);
    }

    #[test]
    fn rl05_the_cap_counts_from_the_baseline_and_closes_the_key_rung() {
        assert_eq!(registry(json!(null)).ladder.key_cap_usd, 10.0, "D3's default");
        let reg = registry(json!({"keyCapUsd": 2.5}));
        assert_eq!(reg.ladder.key_cap_usd, 2.5);
        let step = Step { rung: Rung::ApiKey, to: on("claude:key", "opus") };
        // $40 spent on subscriptions before: only what follows counts.
        let k = KeySpend::enter(&step, 40.0, reg.ladder.key_cap_usd);
        assert_eq!((k.spent(40.0), k.spent(41.5)), (0.0, 1.5));
        assert_eq!(k.capped(42.49), None, "under the cap");
        assert!(keys_open(Harness::Claude, false, Some(42.49), Some(&k)));
        let reason = k.capped(42.5).expect("at the cap");
        assert!(reason.contains("$2.50 spent on claude:key") && reason.contains("cap $2.50"), "{reason}");
        assert!(!keys_open(Harness::Claude, false, Some(42.5), Some(&k)), "at the cap the key rung closes");
        assert!(!keys_open(Harness::Claude, true, None, None), "a spend that cannot be read opens no key rung");
        // The notice names the account, its rung and the cap.
        assert_eq!(notice(&step, 2.5), "Continuing on claude:key (opus): an API-key account, billed per use, up to $2.50 this session.");
        assert_eq!(notice(&Step { rung: Rung::SameHarness, to: on("claude:b", "opus") }, 2.5), "Continuing on claude:b (opus): another subscription of the same harness.");
    }

    #[test]
    fn rl05_attended_codex_never_gets_a_key_rung() {
        let reg = registry(json!(null));
        let spent = usage(&[("claude", None), ("claude:b", None), ("codex", None), ("codex:t", None)]);
        let from = on("codex", "gpt-5.4");
        assert!(!keys_open(Harness::Codex, true, Some(0.0), None), "even with a reading");
        assert_eq!(reg.ladder.next(&reg, &spent, &from, keys_open(Harness::Codex, true, None, None), 2_000), None);
        // Unattended Codex reads its spend from the engine: it climbs on.
        let keys = keys_open(Harness::Codex, false, Some(3.0), None);
        assert_eq!(reg.ladder.next(&reg, &spent, &from, keys, 2_000).map(|s| (s.rung, s.to.instance)), Some((Rung::ApiKey, "codex:key".into())));
        // Attended Claude Code reads it from the mod.
        assert!(keys_open(Harness::Claude, true, Some(3.0), None));
    }
}
