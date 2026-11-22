//! Slugs, and the links that carry them. Wherever an artifact or a run is
//! named, a link that carries one does just as well — the card page, the CDN
//! URL, anything krowk printed.

use crate::error::{fail, Error};

pub const KIND_ARTIFACT: &str = "art";
pub const KIND_RUN: &str = "run";

const SLUG_LENGTH: usize = 24;

fn noun(kind: &str) -> &str {
    match kind {
        KIND_ARTIFACT => "artifact",
        KIND_RUN => "run",
        other => other,
    }
}

fn failure(kind: &str) -> String {
    format!("bad_{}", noun(kind))
}

fn example(kind: &str) -> String {
    if kind == KIND_ARTIFACT {
        return "the artifact slug, like art_…, or the link krowk handed back, like https://krowk.com/a/art_…".into();
    }
    format!("the {} slug, like {kind}_…, or a link carrying it", noun(kind))
}

fn article(noun: &str) -> String {
    match noun.chars().next() {
        None => "one".into(),
        Some(c) if "aeiou".contains(c) => format!("an {noun}"),
        Some(_) => format!("a {noun}"),
    }
}

fn is_base36(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

fn slugs_in(kind: &str, tokens: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for token in tokens {
        let Some(rest) = token.strip_prefix(&format!("{kind}_")) else { continue };
        if rest.len() < SLUG_LENGTH || !rest.chars().all(is_base36) {
            continue;
        }
        let slug = format!("{kind}_{rest}");
        if !found.contains(&slug) {
            found.push(slug);
        }
    }
    found
}

/// Reads what a caller typed where a slug of `kind` belongs. A bare word is
/// taken as the slug; anything shaped like a link is searched for exactly one
/// slug of that kind. "" stays "" — the flag was not given.
pub fn parse_slug(kind: &str, input: &str) -> Result<String, Error> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        if input.is_empty() {
            return Ok(String::new());
        }
        return Err(fail(
            &failure(kind),
            format!("a blank {n} is not one — pass the {n} slug, or a link that carries it", n = noun(kind)),
        ));
    }
    if !trimmed.contains(['/', ':', '.']) {
        return Ok(trimmed.to_string());
    }

    let lower = trimmed.to_lowercase();
    let tokens: Vec<&str> = lower.split(|c: char| !is_base36(c) && c != '_').filter(|t| !t.is_empty()).collect();
    let found = slugs_in(kind, &tokens);
    match found.len() {
        1 => Ok(found.into_iter().next().unwrap()),
        0 => {
            for other in [KIND_ARTIFACT, KIND_RUN] {
                if other == kind {
                    continue;
                }
                if let Some(elsewhere) = slugs_in(other, &tokens).first() {
                    return Err(fail(
                        &failure(kind),
                        format!(
                            "that link names {} — `{elsewhere}` — and this takes {}; pass the {} slug, or a link that carries one",
                            article(noun(other)),
                            article(noun(kind)),
                            noun(kind)
                        ),
                    ));
                }
            }
            Err(fail(&failure(kind), format!("that carries no {} slug — pass {}", noun(kind), example(kind))))
        }
        _ => Err(fail(
            &failure(kind),
            format!(
                "that carries more than one {} — `{}` — so which one is meant is a guess; pass the one you want",
                noun(kind),
                found.join("`, `")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ART: &str = "art_abcdefghijklmnopqrstuvwx";

    #[test]
    fn a_bare_word_is_the_slug_and_absent_stays_absent() {
        assert_eq!(parse_slug(KIND_ARTIFACT, " art_x ").unwrap(), "art_x");
        assert_eq!(parse_slug(KIND_ARTIFACT, "").unwrap(), "");
        assert_eq!(parse_slug(KIND_ARTIFACT, "  ").unwrap_err().code(), "bad_artifact");
    }

    #[test]
    fn a_link_yields_the_one_slug_it_carries() {
        let card = format!("https://krowk.com/a/{}", ART.to_uppercase());
        assert_eq!(parse_slug(KIND_ARTIFACT, &card).unwrap(), ART);
        let cdn = format!("https://cdn.krowkusercontent.com/weur/ws_x/{ART}/shot.png");
        assert_eq!(parse_slug(KIND_ARTIFACT, &cdn).unwrap(), ART);
    }

    #[test]
    fn a_link_naming_the_other_kind_or_two_slugs_is_refused() {
        let run = "https://krowk.com/r/run_abcdefghijklmnopqrstuvwx";
        assert!(parse_slug(KIND_ARTIFACT, run).unwrap_err().fix().contains("names a run"));
        let two = format!("https://x/{ART}/art_zzzzzzzzzzzzzzzzzzzzzzzz");
        assert!(parse_slug(KIND_ARTIFACT, &two).unwrap_err().fix().contains("more than one"));
        assert!(parse_slug(KIND_RUN, "https://krowk.com/").unwrap_err().fix().starts_with("that carries no run slug"));
    }
}
