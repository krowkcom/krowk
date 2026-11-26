//! `gruntas providers list --check` (what `gruntas status` was until it
//! became the agent tree): whether each Claude Code or Codex account can run a turn
//! here — the implicit ones and the configured ones, one row each (an
//! instance gruntas would run itself is not offered: D6): its name, its
//! kind, its readiness, where its credential comes from, and what fixes it.
//! The answer is `gruntas_harness::readiness`, the same check `providers
//! list`, `doctor` and a model switch use, so the three never disagree.
//!
//! It exits 0 when at least one instance is ready, and 3 (`none_ready`)
//! when none is, so a script can ask "can gruntas run anything here?" in one
//! call. `--json` is the table as data:
//!
//! ```json
//! {"ready": 1, "instances": [{"instance": "anthropic", "kind": "claude-code",
//!   "state": "ready", "ready": true, "source": "env ANTHROPIC_API_KEY, handed to Claude Code",
//!   "fix": null, "var": null, "reason": null}, …]}
//! ```
//!
//! Each row also says what the account is signed in as and has left
//! (Rollover for launched sessions D8): `login` (null until gruntas has
//! seen it), `plan` (the subscription, where the vendor names it), `usage`
//! (each window's percent `left` and `resetsAtMs`, from the usage records
//! rollover keeps; empty when none) and `sharedWith` (the other accounts
//! signed in as the same login, between which a move gains nothing).
//! `config_dir` is the account's config home, as `providers list` says it
//! (null for an account with none): where the pi extension records the
//! usage a turn on it reports.
//!
//! `state` is one of `ready`, `key_not_set`, `not_signed_in`, `expired`,
//! `not_installed`, `unknown`; `var` is set for `key_not_set`, `reason` for
//! `unknown`, `fix` whenever the instance is not ready. Every key is on
//! every row. With none ready, the same object is the failure's `details`.
//! No row ever holds a secret: a key is named by its variable.

use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Error};
use gruntas_harness::instances::{InstancesConfig, Registry};
use gruntas_harness::readiness::{self, Report};
use gruntas_harness::usage::Data;
use serde_json::{json, Value};

/// Every Claude Code or Codex account this host has, checked — the vendors
/// in parallel.
pub(super) fn reports(ctx: &Ctx) -> Result<(InstancesConfig, Registry, Vec<Report>), Error> {
    let cfg = super::prompt::load_instances()?;
    let reg = Registry::resolve(&cfg, ctx.io.env);
    let all: Vec<_> = reg.instances.values().filter(|i| i.backend.is_some()).collect();
    let reports = readiness::check_all(&all, &super::providers::credentials_path()?, &neutral_probe(ctx)?);
    Ok((cfg, reg, reports))
}

/// Where a vendor is asked when no repository is trusted: gruntas's own
/// `0700` directory, never the working directory (whose project settings
/// nobody trusted) nor the shared temporary directory (where anyone could
/// plant settings that make a signed-out account look signed in).
pub(super) fn neutral_probe(ctx: &Ctx) -> Result<readiness::Probe, Error> {
    let home = gruntas_api::home::dir(ctx.io.env)?;
    readiness::neutral_dir(&home).map(readiness::Probe::at).map_err(|e| fail("data_dir_unwritable", e))
}

/// The usage records rollover keeps (`usage.json`): every account's login
/// and each login's windows, with what was reported under `registry`'s
/// homes read as their logins' (`Data::link_homes`).
pub(super) fn usage(ctx: &Ctx, registry: &Registry) -> Result<Data, Error> {
    let mut data = gruntas_harness::usage::Usage::in_home(&gruntas_api::home::dir(ctx.io.env)?).read();
    data.link_homes(registry);
    Ok(data)
}

/// What `r`'s account is signed in as and has left (D8), as of `now_ms`.
pub(super) struct Standing {
    pub login: Option<String>,
    pub plan: Option<String>,
    /// Each window's name, percent left and reset.
    pub windows: Vec<(String, Option<f64>, Option<i64>)>,
    /// The other accounts of `all` on the same login.
    pub shared: Vec<String>,
}

pub(super) fn standing(r: &Report, all: &[Report], data: &Data, now_ms: i64) -> Standing {
    let login = |name: &str| data.instance(name).map(|i| i.login.clone());
    let me = login(&r.instance);
    let windows = me.as_deref().map(|l| data.readings(l, now_ms)).unwrap_or_default().into_iter().map(|w| (w.window, w.used_percent.map(|u| (100.0 - u).max(0.0)), w.resets_at_ms)).collect();
    let shared = all.iter().filter(|o| o.instance != r.instance && o.kind == r.kind && me.is_some() && login(&o.instance) == me).map(|o| o.instance.clone()).collect();
    // The vendor's own words, as readiness keeps them: "… a Claude max subscription".
    let plan = match &r.readiness {
        readiness::Readiness::Ready { source } => source.split_once("with a Claude ").and_then(|(_, p)| p.split_once(" subscription")).map(|(p, _)| p.to_string()),
        _ => None,
    };
    Standing { login: me, plan, windows, shared }
}

impl Standing {
    pub fn json(&self) -> Value {
        let usage: Vec<Value> = self.windows.iter().map(|(w, left, at)| json!({ "window": w, "left": left, "resetsAtMs": at })).collect();
        json!({ "login": self.login, "plan": self.plan, "usage": usage, "sharedWith": self.shared })
    }

    /// The 5-hour and 7-day windows: what a picker shows and ranks by.
    fn main_windows(&self) -> impl Iterator<Item = &(String, Option<f64>, Option<i64>)> {
        self.windows.iter().filter(|(w, _, _)| w == "five_hour" || w == "seven_day")
    }

    /// The least the 5-hour or 7-day window has left, in percent: none when
    /// neither is known.
    pub fn least_left(&self) -> Option<f64> {
        self.main_windows().filter_map(|(_, left, _)| *left).reduce(f64::min)
    }

    /// A picker's few words: what the 7-day and 5-hour windows have used,
    /// `90% weekly, 3% 5-hour`.
    pub fn brief(&self) -> String {
        self.used(false)
    }

    /// One line for a person: the login and plan, what the 7-day and
    /// 5-hour windows have used and when they reset, and the accounts
    /// sharing the login.
    pub fn line(&self) -> String {
        let mut out = match (&self.login, &self.plan) {
            (Some(l), Some(p)) => format!("{l} ({p})"),
            (Some(l), None) => l.clone(),
            (None, _) => "login unknown".into(),
        };
        out.push_str(&format!(" · {}", self.used(true)));
        if !self.shared.is_empty() {
            out.push_str(&format!(" · same login as {}", self.shared.join(", ")));
        }
        out
    }

    /// `90% weekly, 3% 5-hour`: each main window's use, the weekly first,
    /// with its reset when `resets`; `usage unknown` when neither is known.
    fn used(&self, resets: bool) -> String {
        let mut windows: Vec<_> = self.main_windows().collect();
        windows.sort_by_key(|(w, _, _)| w != "seven_day");
        let parts: Vec<String> = windows
            .into_iter()
            .map(|(w, left, at)| {
                let name = if w == "seven_day" { "weekly" } else { "5-hour" };
                let used = left.map_or("?".into(), |l| format!("{:.0}%", 100.0 - l));
                let at = at.filter(|_| resets).and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()).map(|t| format!(" (resets {})", t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %H:%M"))).unwrap_or_default();
                format!("{used} {name}{at}")
            })
            .collect();
        if parts.is_empty() { "usage unknown".into() } else { parts.join(", ") }
    }
}

pub(super) fn status(ctx: &mut Ctx) -> Result<(), Error> {
    let (_, registry, reports) = reports(ctx)?;
    let ready = reports.iter().filter(|r| r.readiness.is_ready()).count();
    let (data, now) = (usage(ctx, &registry)?, gruntas_store::now_ms());
    let standings: Vec<Standing> = reports.iter().map(|r| standing(r, &reports, &data, now)).collect();
    let rows = reports.iter().zip(&standings).map(|(r, s)| {
        let mut row = r.json();
        if let (Some(row), Value::Object(more)) = (row.as_object_mut(), s.json()) {
            row.extend(more);
        }
        let home = registry.instances.get(&r.instance).and_then(|i| i.backend.as_ref()).and_then(|b| b.home.as_ref());
        row["config_dir"] = json!(home.map(|p| p.display().to_string()));
        row
    });
    let data = json!({ "ready": ready, "instances": rows.collect::<Vec<Value>>() });
    let none = || fail("none_ready", format!("none of the {} instances can run a turn here — each row's fix says what makes it ready", reports.len()));
    if ctx.format != Format::Human {
        // The rows ride on the failure, so a script reads them either way;
        // a person has just been shown them as the table.
        if ready == 0 {
            let mut err = none();
            err.body.insert("details".into(), data);
            return Err(err);
        }
        let summary = format!("{ready} of {} instances ready", reports.len());
        return super::sessions::emit_data(ctx, data.clone(), summary);
    }
    let width = |f: &dyn Fn(&Report) -> usize| reports.iter().map(f).max().unwrap_or(0);
    let label = |r: &Report| r.label;
    let (wn, wk, ws) = (width(&|r| r.instance.len()), width(&|r| label(r).len()), width(&|r| r.readiness.label().len()));
    let out = &mut *ctx.io.stdout;
    for (r, s) in reports.iter().zip(&standings) {
        // What failed, for a check that could not tell; else the fix.
        let why = match &r.readiness {
            readiness::Readiness::Unknown { reason } => Some(reason),
            _ => r.fix.as_ref(),
        };
        let fix = why.map(|f| format!("  — {f}")).unwrap_or_default();
        let _ = writeln!(out, "{:<wn$}  {:<wk$}  {:<ws$}  {}{fix}", r.instance, label(r), r.readiness.label(), r.source);
        let _ = writeln!(out, "{:<wn$}  {}", "", s.line());
    }
    if ready == 0 {
        return Err(none());
    }
    let _ = writeln!(out, "{ready} of {} instances ready", reports.len());
    Ok(())
}
