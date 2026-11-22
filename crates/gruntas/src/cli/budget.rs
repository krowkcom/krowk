//! `gruntas sessions budget`: whether a session is still inside a spend limit,
//! judged by what the provider metered — never by what a request asked for.
//!
//! A request's `max_tokens` is a wish, not a meter: on 2026-09-10 two
//! qwen3.5-plus calls sent with `max_tokens: 1200` completed 1,379 and 3,422
//! tokens, and a guard summing the caps would have under-budgeted by up to
//! 3x. So the guard sums the usage blocks the store holds, for the session
//! and every subagent it spawned, and prices them the way `sessions show`
//! does. `--max-tokens` counts generated tokens — output and reasoning, the
//! part a cap is meant to bound and the part that overshoots; input and
//! cache tokens are reported and priced, not counted against it.
//!
//! The check is current: the session's transcripts are re-read first when
//! they moved (unless another import holds the store, which the report
//! says). A provider-ledger row nobody saw an answer to is in the ledger's
//! own session — nothing ties it to the run that sent it — so budget the
//! ledger session to hold a provider's metered total to a limit.
//!
//! gruntas runs no model, so it cancels nothing: tripping exits 4 with
//! `budget_exceeded`, and whatever called the guard (a hook, a wrapper, a
//! CI step) is what stops the run. Priced costs stay unrounded from the
//! turn to the comparison; a cost the source reported is stored to the
//! micro-dollar, finer than any provider bills. A cost gruntas cannot price
//! trips a `--max-usd` guard rather than passing it: an unknown spend is
//! not a spend inside the limit.

use super::sessions::{self, Priced, Refresh};
use super::Ctx;
use crate::output::Format;
use crate::pricing;
use gruntas_api::{fail, Error};
use gruntas_store::{SessionDetail, TurnDetail};
use serde::Serialize;
use serde_json::{json, Value};

/// The limits one check holds a session to; either may be absent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub usd: Option<f64>,
    pub tokens: Option<i64>,
}

/// Every metered token of the counted turns, the reasoning split kept apart
/// from output even where both price the same today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Metered {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Output and reasoning: what `--max-tokens` holds to its limit.
    pub generated: i64,
    pub total: i64,
}

impl Metered {
    fn add(&mut self, t: &TurnDetail) {
        let n = |x: i64| x.max(0);
        self.input += n(t.input);
        self.output += n(t.output);
        self.reasoning += n(t.reasoning);
        self.cache_read += n(t.cache_read);
        self.cache_write += n(t.cache_write);
        self.generated = self.output + self.reasoning;
        self.total = self.input + self.output + self.reasoning + self.cache_read + self.cache_write;
    }

    fn merge(&mut self, o: Metered) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.generated += o.generated;
        self.total += o.total;
    }

    /// The turns a session's own cost counts: a ledger row a transcript or an
    /// earlier export already holds is metered there.
    pub fn of(d: &SessionDetail) -> Metered {
        let mut m = Metered::default();
        for t in d.turns.iter().filter(|t| !sessions::is_observed(t)) {
            m.add(t);
        }
        m
    }
}

/// One limit the session is past.
#[derive(Debug, Clone, PartialEq)]
pub enum Trip {
    Tokens { metered: i64, limit: i64 },
    Usd { cost: f64, limit: f64 },
    /// Some of the cost has no price; `known` is the part that has one, a
    /// lower bound on the whole.
    UsdUnknown { unpriced: Vec<String>, known: f64 },
}

impl Trip {
    fn json(&self) -> Value {
        match self {
            Trip::Tokens { metered, limit } => json!({ "limit": "max_tokens", "generated": metered, "max": limit }),
            Trip::Usd { cost, limit } => json!({ "limit": "max_usd", "cost_usd": cost, "max": limit }),
            Trip::UsdUnknown { unpriced, known } => {
                json!({ "limit": "max_usd", "cost_usd": Value::Null, "cost_usd_at_least": known, "unpriced": unpriced })
            }
        }
    }

    fn sentence(&self) -> String {
        match self {
            Trip::Tokens { metered, limit } => format!("{metered} tokens generated, over --max-tokens {limit}"),
            Trip::Usd { cost, limit } => {
                format!("{} metered, over --max-usd {}", sessions::format_cost_precise(*cost), sessions::format_cost_precise(*limit))
            }
            Trip::UsdUnknown { unpriced, known } => {
                format!("the cost is unknown — at least {}, with no price for {}", sessions::format_cost_precise(*known), unpriced.join(", "))
            }
        }
    }
}

/// Every limit the metered usage is past. Strictly over trips; at the limit
/// is inside it. Compared unrounded.
pub fn check(m: &Metered, cost: &Priced, l: &Limits) -> Vec<Trip> {
    let mut out = Vec::new();
    if let Some(limit) = l.tokens
        && m.generated > limit
    {
        out.push(Trip::Tokens { metered: m.generated, limit });
    }
    if let Some(limit) = l.usd {
        match cost.total() {
            Some(c) if c > limit => out.push(Trip::Usd { cost: c, limit }),
            Some(_) => {}
            None => out.push(Trip::UsdUnknown { unpriced: cost.missing().into_iter().collect(), known: cost.known_usd() }),
        }
    }
    out
}

fn descendants(conn: &gruntas_store::Connection, id: &str) -> Result<Vec<String>, Error> {
    gruntas_store::descendant_session_ids(conn, id).map_err(|e| fail("store_unavailable", e.message().to_string()))
}

/// The session the argument names, with its transcripts — and every
/// subagent's — brought up to date first. A session the store has never
/// seen is looked for by its agent's own id (a Claude or opencode session
/// id), so a hook's first check in a fresh session answers instead of
/// failing.
fn current_session(ctx: &Ctx, args: &[String]) -> Result<(String, Refresh), Error> {
    let conn = sessions::open_store(ctx)?;
    let mut imported = false;
    let id = match sessions::resolve_arg(ctx, &conn, args, "budget") {
        Ok(id) => id,
        Err(e) if e.code() == "no_session" && !args.is_empty() => {
            drop(conn);
            let reference = args[0].trim().to_string();
            for provider in [gruntas_import::PROVIDER_CLAUDE, gruntas_import::PROVIDER_OPENCODE] {
                imported |= sessions::refresh_session(ctx, provider, std::slice::from_ref(&reference))? == Refresh::Imported;
            }
            sessions::resolve_arg(ctx, &sessions::open_store(ctx)?, args, "budget")?
        }
        Err(e) => return Err(e),
    };
    let conn = sessions::open_store(ctx)?;
    let s = sessions::load_by_id(ctx, &conn, &id)?.session;
    let mut ids = vec![s.foreign_session_id.clone()];
    for child in descendants(&conn, &id)? {
        ids.push(sessions::load_by_id(ctx, &conn, &child)?.session.foreign_session_id);
    }
    drop(conn);
    match sessions::refresh_session(ctx, &s.binding_provider, &ids)? {
        Refresh::Current if imported => Ok((id, Refresh::Imported)),
        r => Ok((id, r)),
    }
}

fn limits(ctx: &Ctx) -> Result<Limits, Error> {
    let l = given_limits(ctx)?;
    if l.usd.is_none() && l.tokens.is_none() {
        return Err(fail("bad_flag", "`gruntas sessions budget` needs a limit: --max-usd <dollars>, --max-tokens <count>, or both"));
    }
    Ok(l)
}

/// `--max-usd` and `--max-tokens` as given, either or both absent: what
/// `gruntas sessions budget` checks a session against, and what `gruntas -p`
/// holds one to before each turn and as each call is metered.
pub(super) fn given_limits(ctx: &Ctx) -> Result<Limits, Error> {
    // A limit given blank — `--max-usd "$UNSET"` — is a mistake to name, not
    // a check to switch off.
    for (name, v) in [("max-usd", &ctx.f.max_usd), ("max-tokens", &ctx.f.max_tokens)] {
        if ctx.f.given.contains(name) && v.trim().is_empty() {
            return Err(fail("bad_flag", format!("--{name} was given with no value — pass the limit, or leave the flag out")));
        }
    }
    let usd = match ctx.f.max_usd.trim() {
        "" => None,
        v => Some(v.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0).ok_or_else(|| {
            fail("bad_flag", format!("--max-usd {v:?} is not an amount of dollars — pass a number like 0.50"))
        })?),
    };
    let tokens = match ctx.f.max_tokens.trim() {
        "" => None,
        v => Some(v.parse::<i64>().ok().filter(|x| *x >= 0).ok_or_else(|| {
            fail("bad_flag", format!("--max-tokens {v:?} is not a count of tokens — pass a whole number like 200000"))
        })?),
    };
    Ok(Limits { usd, tokens })
}

pub fn budget(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    sessions::check_os()?;
    let l = limits(ctx)?;
    let (id, refreshed) = current_session(ctx, args)?;
    let conn = sessions::open_store(ctx)?;
    let d = sessions::load_by_id(ctx, &conn, &id)?;
    let (_, mut cost) = sessions::price_turns(ctx, &d);
    let mut metered = Metered::of(&d);
    let children = descendants(&conn, &id)?;
    for child in &children {
        let child = sessions::load_by_id(ctx, &conn, child)?;
        cost.merge(sessions::price_turns(ctx, &child).1);
        metered.merge(Metered::of(&child));
    }
    let children = children.len();
    let trips = check(&metered, &cost, &l);
    let s = &d.session;
    let mut report = json!({
        "id": s.id,
        "title": s.title,
        "subagents": children,
        "metered": metered,
        "cost_usd": cost.total(),
        "cost_display": cost.total().map_or("—".to_string(), sessions::format_cost_precise),
        "limits": { "max_usd": l.usd, "max_tokens": l.tokens },
        "within": trips.is_empty(),
        // Now, when the transcripts were just checked; else the last import.
        "as_of_ms": if refreshed == Refresh::NotRefreshable { s.time_updated } else { gruntas_store::now_ms() },
        "refreshed": match refreshed {
            Refresh::Current => json!("current"),
            Refresh::Imported => json!("imported"),
            Refresh::NotRefreshable => Value::Null,
        },
    });
    if !cost.bases.is_empty() {
        report["priced_with"] = json!(pricing::basis_note(&cost.bases));
    }
    if !trips.is_empty() {
        report["tripped"] = Value::Array(trips.iter().map(Trip::json).collect());
        let why: Vec<String> = trips.iter().map(Trip::sentence).collect();
        let mut err = fail(
            "budget_exceeded",
            format!(
                "session {} is over budget: {} — stop the run; gruntas cancels nothing, and the provider keeps billing whatever is still running",
                s.id,
                why.join("; ")
            ),
        );
        err.body.insert("details".into(), report);
        return Err(err);
    }
    if ctx.format != Format::Human {
        let summary = format!("{} — within budget", sessions::display_title(&sessions::cell(&s.title)));
        return sessions::emit_data(ctx, report, summary);
    }
    let mut line = format!("within budget  {} tokens generated", metered.generated);
    if let Some(t) = l.tokens {
        line += &format!(" of {t}");
    }
    if let (Some(c), Some(u)) = (cost.total(), l.usd) {
        line += &format!("  {} of {}", sessions::format_cost_precise(c), sessions::format_cost_precise(u));
    }
    if refreshed == Refresh::NotRefreshable {
        line += "  (as of the last import: this source is not re-read per session)";
    }
    let _ = writeln!(ctx.io.stdout, "{line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(usd: Option<f64>) -> Priced {
        let mut p = Priced::default();
        match usd {
            Some(u) => p.add_known(u),
            None => p.add_unknown("p/m"),
        }
        p
    }

    fn turn(output: i64, reasoning: i64, status: &str) -> TurnDetail {
        TurnDetail { input: 84, output, reasoning, status: status.into(), ..TurnDetail::default() }
    }

    #[test]
    fn a_runaway_reasoning_turn_trips_on_what_was_metered_not_the_cap_it_was_sent_with() {
        // Sent with max_tokens 1200; the provider metered 17 output and 3,405
        // reasoning. Summing the cap would say 84 + 1200 — inside 2,000.
        let d = SessionDetail { turns: vec![turn(17, 3405, "unobserved"), turn(1379, 0, "observed")], ..SessionDetail::default() };
        let m = Metered::of(&d);
        assert_eq!((m.output, m.reasoning, m.generated, m.total), (17, 3405, 3422, 84 + 17 + 3405), "the observed row is metered in its transcript");
        let trips = check(&m, &priced(Some(0.0)), &Limits { tokens: Some(2000), usd: None });
        assert_eq!(trips, vec![Trip::Tokens { metered: 3422, limit: 2000 }]);
        assert!(check(&m, &priced(Some(0.0)), &Limits { tokens: Some(3422), usd: None }).is_empty(), "at the limit is inside it");
    }

    #[test]
    fn dollars_compare_unrounded_and_an_unknown_cost_trips() {
        // Three Zen deepseek turns: 24.08 + 58.94 + 40.264 µ$. Rounded to four
        // places the total and a $0.000123 limit both read $0.0001.
        let mut p = Priced::default();
        for usd in [0.000_024_08, 0.000_058_94, 0.000_040_264] {
            p.add_known(usd);
        }
        assert_eq!(p.total(), Some(0.000_024_08 + 0.000_058_94 + 0.000_040_264), "no rounding inside the roll-up");
        let limits = Limits { usd: Some(0.000_123), tokens: None };
        assert!(matches!(check(&Metered::default(), &p, &limits)[..], [Trip::Usd { .. }]), "$0.000123284 is over $0.000123");
        assert!(check(&Metered::default(), &p, &Limits { usd: Some(0.000_124), tokens: None }).is_empty());
        let unknown = check(&Metered::default(), &priced(None), &limits);
        assert_eq!(unknown, vec![Trip::UsdUnknown { unpriced: vec!["p/m".into()], known: 0.0 }]);
    }
}
