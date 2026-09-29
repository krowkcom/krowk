//! One reasoning-effort ladder for every provider (R-PROV-2): `none`,
//! `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. A person asks for a
//! rung; each model takes some of the rungs, and the one asked for is
//! mapped onto the nearest it takes, so `--effort max` means "as hard as
//! this model goes" on any model rather than an error on most of them.
//!
//! Which rungs a model takes is the models.dev catalog's to say — its
//! `reasoning_options` effort values — and, for a model the catalog does not
//! know, the family's row in `FAMILIES` below. A model that does not reason,
//! or reasons with no effort to choose, is sent no effort at all.
//!
//! The mapping, in order:
//!
//! 1. A rung the model takes is sent as it is.
//! 2. Asking for any thought at all never lands on `none`: switching
//!    reasoning off is a different request from asking for a little of it.
//!    Only a model whose one rung is `none` gets it.
//! 3. Otherwise the nearest rung, and between two equally near the higher:
//!    a model that thinks a little more than asked costs a little more; one
//!    that thinks less may not finish the task.

use crate::catalog::ModelInfo;
use crate::protocol::Effort;

/// The ladder and a rung's name live with `Effort` in the protocol, which
/// `krowk-client` declares (R-CLIENT-1); the mapping onto a model is here.
pub use crate::protocol::LADDER;

fn rung(e: Effort) -> i32 {
    LADDER.iter().position(|r| *r == e).expect("every effort is on the ladder") as i32
}

/// The rungs a family takes when the catalog does not know the model: what
/// the catalog lists for the family's current models, narrowed to what every
/// one of them takes, so a guess is never refused. A family that is not
/// here, or whose row is empty, is sent no effort.
pub const FAMILIES: &[(&str, &[Effort])] = {
    use Effort::*;
    &[
        ("gpt", &[Low, Medium, High]),
        ("gpt-mini", &[Low, Medium, High]),
        ("gpt-nano", &[Low, Medium, High]),
        ("gpt-codex", &[Low, Medium, High]),
        ("gpt-codex-spark", &[None, Low, Medium, High, Xhigh]),
        ("gpt-pro", &[High]),
        ("gpt-luna", &[None, Low, Medium, High, Xhigh, Max]),
        ("gpt-sol", &[None, Low, Medium, High, Xhigh, Max]),
        ("gpt-terra", &[None, Low, Medium, High, Xhigh, Max]),
        ("gpt-astra", &[Low, Medium, High, Xhigh, Max]),
        ("gpt-oss", &[Low, Medium, High]),
        ("o", &[Low, Medium, High]),
        ("o-mini", &[Low, Medium, High]),
        ("o-pro", &[Low, Medium, High]),
        // grok-4's reasoning models refuse `reasoning_effort` outright; the
        // catalog names the ones that take it.
        ("grok", &[]),
        ("grok-build", &[]),
        ("claude-opus", &[Low, Medium, High]),
        ("claude-sonnet", &[]),
        ("claude-haiku", &[]),
        ("claude-fable", &[Low, Medium, High, Xhigh, Max]),
    ]
};

/// The rungs this model takes: the catalog's when it knows the model,
/// else its family's. Empty when it takes none.
pub fn supported(info: Option<&ModelInfo>, family: Option<&str>) -> Vec<Effort> {
    if let Some(info) = info {
        return if info.reasoning { info.efforts.clone() } else { Vec::new() };
    }
    let family = family.unwrap_or_default();
    FAMILIES.iter().find(|(f, _)| *f == family).map(|(_, e)| e.to_vec()).unwrap_or_default()
}

/// The rung a model is sent for the one asked: none when it takes none.
pub fn map(want: Effort, takes: &[Effort]) -> Option<Effort> {
    if takes.contains(&want) {
        return Some(want);
    }
    let candidates: Vec<Effort> = if want == Effort::None { takes.to_vec() } else { takes.iter().copied().filter(|e| *e != Effort::None).collect() };
    let candidates = if candidates.is_empty() { takes.to_vec() } else { candidates };
    candidates.into_iter().min_by_key(|e| ((rung(*e) - rung(want)).abs(), -rung(*e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effort::*;

    /// Parses a row of the table below: a rung per ladder step, `-` for none sent.
    fn row(s: &str) -> Vec<Option<Effort>> {
        s.split_whitespace().map(|w| if w == "-" { Option::None } else { Some(Effort::parse(w).unwrap()) }).collect()
    }

    #[test]
    fn r_prov_2_the_effort_ladder_maps_onto_every_catalog_family() {
        // Each family as the models.dev catalog lists its models' effort
        // values (openai, xai and anthropic, 2026-09), and what each of the
        // seven rungs — none minimal low medium high xhigh max — is sent as.
        let table: &[(&str, &[Effort], &str)] = &[
            ("gpt (gpt-5.4)", &[None, Low, Medium, High, Xhigh], "none low low medium high xhigh xhigh"),
            ("gpt (gpt-5)", &[Minimal, Low, Medium, High], "minimal minimal low medium high high high"),
            ("gpt (gpt-5.1)", &[None, Low, Medium, High], "none low low medium high high high"),
            ("gpt-mini", &[Minimal, Low, Medium, High], "minimal minimal low medium high high high"),
            ("gpt-nano", &[None, Low, Medium, High, Xhigh], "none low low medium high xhigh xhigh"),
            ("gpt-codex", &[None, Low, Medium, High, Xhigh], "none low low medium high xhigh xhigh"),
            ("gpt-codex (one rung)", &[Medium], "medium medium medium medium medium medium medium"),
            ("gpt-codex-spark", &[None, Low, Medium, High, Xhigh], "none low low medium high xhigh xhigh"),
            ("gpt-pro", &[Medium, High, Xhigh], "medium medium medium medium high xhigh xhigh"),
            ("gpt-pro (one rung)", &[High], "high high high high high high high"),
            ("gpt-luna", &[None, Low, Medium, High, Xhigh, Max], "none low low medium high xhigh max"),
            ("gpt-sol", &[None, Low, Medium, High, Xhigh, Max], "none low low medium high xhigh max"),
            ("gpt-terra", &[None, Low, Medium, High, Xhigh, Max], "none low low medium high xhigh max"),
            ("gpt-astra", &[Low, Medium, High, Xhigh, Max], "low low low medium high xhigh max"),
            ("o / o-mini / o-pro", &[Low, Medium, High], "low low low medium high high high"),
            ("grok (grok-4.7)", &[Low, Medium, High, Xhigh], "low low low medium high xhigh xhigh"),
            ("grok (grok-4.3)", &[None, Low, Medium, High], "none low low medium high high high"),
            ("grok (grok-4.20 reasoning)", &[], "- - - - - - -"),
            ("grok-build", &[], "- - - - - - -"),
            ("claude-opus", &[Low, Medium, High, Xhigh, Max], "low low low medium high xhigh max"),
            ("claude-opus (4.6)", &[Low, Medium, High, Max], "low low low medium high max max"),
            ("claude-sonnet", &[Low, Medium, High, Max], "low low low medium high max max"),
            ("claude-haiku", &[], "- - - - - - -"),
            ("claude-fable", &[Low, Medium, High, Xhigh, Max], "low low low medium high xhigh max"),
        ];
        for (family, takes, want) in table {
            let got: Vec<Option<Effort>> = LADDER.iter().map(|e| map(*e, takes)).collect();
            assert_eq!(got, row(want), "{family}: {takes:?}");
        }
        // Every family the fallback table names maps without panicking, and
        // only onto rungs it lists.
        for (family, takes) in FAMILIES {
            for e in LADDER {
                let m = map(e, &supported(Option::None, Some(family)));
                assert!(m.is_none_or(|m| takes.contains(&m)), "{family} {e:?}");
            }
        }
        // The catalog outranks the family; a model that does not reason is
        // sent nothing whatever its family.
        let info = ModelInfo { reasoning: true, efforts: vec![Xhigh], ..ModelInfo::default() };
        assert_eq!(map(Low, &supported(Some(&info), Some("gpt"))), Some(Xhigh));
        let plain = ModelInfo { reasoning: false, efforts: vec![Low], ..ModelInfo::default() };
        assert!(supported(Some(&plain), Some("gpt")).is_empty());
        assert_eq!(Effort::parse(" XHigh "), Some(Xhigh));
        assert_eq!(Effort::parse("extreme"), Option::None);
    }
}
