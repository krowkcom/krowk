//! `krowk status`: whether each provider instance can run a turn here —
//! the implicit ones and the configured ones, one row each: its name, its
//! kind, its readiness, where its credential comes from, and what fixes it.
//! The answer is `krowk_harness::readiness`, the same check `providers
//! list`, `doctor` and a model switch use, so the three never disagree.
//!
//! It exits 0 when at least one instance is ready, and 3 (`none_ready`)
//! when none is, so a script can ask "can krowk run anything here?" in one
//! call. `--json` is the table as data:
//!
//! ```json
//! {"ready": 1, "instances": [{"instance": "anthropic", "kind": "anthropic-api",
//!   "state": "ready", "ready": true, "source": "$ANTHROPIC_API_KEY",
//!   "fix": null, "var": null, "reason": null}, …]}
//! ```
//!
//! `state` is one of `ready`, `key_not_set`, `not_signed_in`, `expired`,
//! `not_installed`, `unknown`; `var` is set for `key_not_set`, `reason` for
//! `unknown`, `fix` whenever the instance is not ready. Every key is on
//! every row. With none ready, the same object is the failure's `details`.
//! No row ever holds a secret: a key is named by its variable.

use super::Ctx;
use crate::output::Format;
use krowk_api::{fail, Error};
use krowk_harness::instances::{InstancesConfig, Registry};
use krowk_harness::readiness::{self, Report};
use serde_json::{json, Value};

/// Every instance this host has, checked — the vendors in parallel.
pub(super) fn reports(ctx: &Ctx) -> Result<(InstancesConfig, Registry, Vec<Report>), Error> {
    let cfg = super::prompt::load_instances()?;
    let reg = Registry::resolve(&cfg, ctx.io.env);
    let all: Vec<_> = reg.instances.values().collect();
    let reports = readiness::check_all(&all, &super::providers::credentials_path(), &neutral_probe(ctx)?);
    Ok((cfg, reg, reports))
}

/// Where a vendor is asked when no repository is trusted: krowk's own
/// `0700` directory, never the working directory (whose project settings
/// nobody trusted) nor the shared temporary directory (where anyone could
/// plant settings that make a signed-out account look signed in).
pub(super) fn neutral_probe(ctx: &Ctx) -> Result<readiness::Probe, Error> {
    let data = krowk_harness::log::sessions_dir(ctx.io.env)
        .and_then(|d| d.parent().map(std::path::Path::to_path_buf))
        .ok_or_else(|| fail("no_home", "krowk has no data directory to ask a vendor from — set HOME or XDG_DATA_HOME"))?;
    readiness::neutral_dir(&data).map(readiness::Probe::at).map_err(|e| fail("data_dir_unwritable", e))
}

pub(super) fn status(ctx: &mut Ctx) -> Result<(), Error> {
    let (_, _, reports) = reports(ctx)?;
    let ready = reports.iter().filter(|r| r.readiness.is_ready()).count();
    let data = json!({ "ready": ready, "instances": reports.iter().map(Report::json).collect::<Vec<Value>>() });
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
    let (wn, wk, ws) = (width(&|r| r.instance.len()), width(&|r| r.kind.len()), width(&|r| r.readiness.label().len()));
    let out = &mut *ctx.io.stdout;
    for r in &reports {
        // What failed, for a check that could not tell; else the fix.
        let why = match &r.readiness {
            readiness::Readiness::Unknown { reason } => Some(reason),
            _ => r.fix.as_ref(),
        };
        let fix = why.map(|f| format!("  — {f}")).unwrap_or_default();
        let _ = writeln!(out, "{:<wn$}  {:<wk$}  {:<ws$}  {}{fix}", r.instance, r.kind, r.readiness.label(), r.source);
    }
    if ready == 0 {
        return Err(none());
    }
    let _ = writeln!(out, "{ready} of {} instances ready", reports.len());
    Ok(())
}
