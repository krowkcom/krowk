//! What `krowk doctor` checks about the person's setup beyond the store, the
//! prices and the providers (D9, R-EASE-8): the krowk account, this machine's
//! registration for sync, the always-on service, repository trust, the
//! recovery kit, and the plan with its machine limit. Each is one line with
//! its state and, when it needs fixing, the visible command that fixes it.
//!
//! The decisions are plain functions of what was read, so each has a passing
//! and a failing test; reading is `Machine::here` and friends. The TUI's
//! status line asks for the one most important problem (`top_problem`),
//! from what is on disk alone: it never waits on the registry.
//!
//! The recovery kit's line is the doctor's alone. In the TUI the once-per-
//! session reminder is the status line's own (`Options::no_recovery_kit`,
//! ticket 02), so `top_problem` never names it: one owner, never twice.

use super::Ctx;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Pass,
    /// Worth knowing; nothing is broken, so the exit status is unchanged.
    Warn,
    /// Needs fixing: doctor exits non-zero.
    Fail,
    Skip,
}

impl Status {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Check {
    pub name: &'static str,
    pub status: Status,
    pub message: String,
    /// The command that fixes it, for anything but a pass.
    pub fix: Option<String>,
}

impl Check {
    fn new(name: &'static str, status: Status, message: impl Into<String>, fix: Option<&str>) -> Check {
        Check { name, status, message: message.into(), fix: fix.map(str::to_string) }
    }

    pub(super) fn json(&self) -> Value {
        let mut v = json!({ "name": self.name, "status": self.status.as_str(), "message": self.message });
        if let Some(fix) = &self.fix {
            v["fix"] = json!(fix);
        }
        v
    }

    /// "message — fix": what the status line and the doctor's lines say.
    pub(super) fn said(&self) -> String {
        match &self.fix {
            Some(fix) => format!("{} — {fix}", self.message),
            None => self.message.clone(),
        }
    }
}

/// What the key lookup said, when doctor asked the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Lookup {
    /// Not asked: the status line reads the disk only.
    Unchecked,
    Ok { email: String, workspace: String },
    /// The registry could not be reached, so the key is neither good nor bad.
    Offline,
    Rejected(String),
}

pub(super) fn account(signed_in: bool, local: bool, lookup: &Lookup) -> Check {
    const FIX: &str = "krowk login";
    if !signed_in {
        return match local {
            true => Check::new("account", Status::Skip, "Using krowk locally, without an account", Some(FIX)),
            false => Check::new("account", Status::Fail, "Not signed in to krowk", Some(FIX)),
        };
    }
    match lookup {
        Lookup::Rejected(code) => Check::new("account", Status::Fail, format!("The registry rejected this key ({code})"), Some(FIX)),
        Lookup::Ok { email, .. } if !email.is_empty() => Check::new("account", Status::Pass, format!("Signed in as {email}"), None),
        Lookup::Ok { workspace, .. } if !workspace.is_empty() => Check::new("account", Status::Pass, format!("Signed in to {workspace}"), None),
        Lookup::Offline => Check::new("account", Status::Pass, "Signed in (the registry could not be reached to check the key)", None),
        _ => Check::new("account", Status::Pass, "Signed in", None),
    }
}

/// What this machine's own files say of its place on the person's device
/// list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Machine {
    /// No registration to ask about, and why.
    NotApplicable(&'static str),
    Unreadable(String),
    NotRegistered,
    /// A list is kept, and this machine is not on it.
    Removed,
    Registered { name: String, machines: usize, kit: bool },
}

pub(super) fn registered(signed_in: bool, machine: &Machine) -> Check {
    const FIX: &str = "krowk login";
    if !signed_in {
        return Check::new("machine", Status::Skip, "This machine is not registered for sync: there is no account", None);
    }
    match machine {
        Machine::NotApplicable(why) => Check::new("machine", Status::Skip, *why, None),
        Machine::Unreadable(e) => Check::new("machine", Status::Fail, format!("This machine's keys could not be read: {e}"), Some(FIX)),
        Machine::NotRegistered => Check::new("machine", Status::Fail, "This machine is not registered for sync", Some(FIX)),
        Machine::Removed => Check::new("machine", Status::Fail, "This machine is no longer on your device list", Some(FIX)),
        Machine::Registered { name, .. } => Check::new("machine", Status::Pass, format!("This machine is registered as '{name}'"), None),
    }
}

/// `service`: whether the service file is installed, or `None` where this
/// platform has none.
pub(super) fn always_on(signed_in: bool, service: Option<bool>) -> Check {
    const FIX: &str = "krowk machines always-on on";
    match (signed_in, service) {
        (_, None) => Check::new("always_on", Status::Skip, "The always-on service is not available on this platform", None),
        (false, _) => Check::new("always_on", Status::Skip, "The always-on service syncs your sessions, which needs an account", None),
        (true, Some(true)) => Check::new("always_on", Status::Pass, "Always-on service is on", None),
        (true, Some(false)) => Check::new("always_on", Status::Fail, "Always-on service off", Some(FIX)),
    }
}

/// This directory's repository, against the trusted list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Trust {
    NotRepository,
    Trusted(String),
    Untrusted(String),
}

pub(super) fn trusted(t: &Trust) -> Check {
    match t {
        Trust::NotRepository => Check::new("trust", Status::Skip, "This directory is not in a repository", None),
        Trust::Trusted(dir) => Check::new("trust", Status::Pass, format!("{dir} is trusted"), None),
        // Also what a repository reads as once something in it that can run
        // code has changed since it was trusted.
        Trust::Untrusted(dir) => Check::new("trust", Status::Fail, format!("{dir} is not trusted, or something in it that can run code changed"), Some("run `krowk` there and answer its question")),
    }
}

/// D5's rule: fewer than two machines (phones are devices, not machines) and
/// no kit on the list. // ticket 02: the same rule as `recovery::reminder_due`;
/// once that merges, `Machine::here` reads `kit` and `machines` through it.
pub(super) fn recovery_kit(machine: &Machine) -> Check {
    match machine {
        Machine::Registered { kit: true, .. } => Check::new("recovery_kit", Status::Pass, "Recovery kit is on your device list", None),
        Machine::Registered { machines, .. } if *machines >= 2 => Check::new("recovery_kit", Status::Pass, format!("No recovery kit, but you have {machines} machines to stand in for each other"), None),
        Machine::Registered { .. } => Check::new("recovery_kit", Status::Fail, "One machine, no recovery kit: losing it loses your sessions", Some("krowk machines recovery")),
        _ => Check::new("recovery_kit", Status::Skip, "No device list here to hold a recovery kit", None),
    }
}

/// How many machines a plan syncs sessions from (R-SYNC-13); `None` is
/// unlimited.
fn machine_limit(plan: &str) -> Option<usize> {
    match plan {
        "free" => Some(2),
        "business" => None,
        _ => Some(10),
    }
}

/// The plan as the key names it, and the machines on the person's list. The
/// registry counts only machines that hosted in the last 30 days and says
/// so only to a machine it refuses, which keeps no record: a list longer
/// than the limit is a warning, not a failure.
pub(super) fn plan(plan: Option<&str>, machine: &Machine) -> Check {
    let Some(plan) = plan.filter(|p| !p.is_empty()) else {
        return Check::new("plan", Status::Skip, "The plan is not known: the key was not checked", None);
    };
    let name = crate::output::fix::capitalised(plan);
    let machines = match machine {
        Machine::Registered { machines, .. } => Some(*machines),
        _ => None,
    };
    match (machine_limit(plan), machines) {
        (Some(limit), Some(n)) if n > limit => Check::new("plan", Status::Warn, format!("{name} syncs sessions from {limit} machines, and your device list names {n}: the extra ones stay viewers"), Some("krowk billing")),
        (Some(limit), Some(n)) => Check::new("plan", Status::Pass, format!("{name} plan: {n} of {limit} machines"), None),
        (Some(limit), None) => Check::new("plan", Status::Pass, format!("{name} plan: up to {limit} machines"), None),
        (None, _) => Check::new("plan", Status::Pass, format!("{name} plan: unlimited machines"), None),
    }
}

/// The checks that read the disk only, in the order of importance: what the
/// status line picks from, and the first part of the doctor's report.
pub(super) fn local(ctx: &Ctx, signed_in: bool, lookup: &Lookup) -> (Vec<Check>, Machine) {
    let machine = Machine::here(ctx, signed_in);
    let checks = vec![account(signed_in, chose_local(ctx), lookup), registered(signed_in, &machine), always_on(signed_in, service_installed(ctx))];
    (checks, machine)
}

/// The most important problem with this setup, said for the status line:
/// from the disk alone, so it costs the TUI no wait. Only what is broken
/// counts, and only for a person who has an account: one who never signed in
/// is not nagged on every start (doctor says so, and the first run asked),
/// and a key the registry rejects is found by asking it, which is doctor's.
/// Left out on purpose: the recovery kit (the status line's own reminder,
/// once per session), repository trust (the TUI asks it before the first
/// prompt), and the always-on service (a person may have said no).
pub(super) fn top_problem(ctx: &Ctx) -> Option<String> {
    let client = super::agent::new_client(ctx).ok()?;
    let signed_in = client.authenticated() && ctx.env("KROWK_TOKEN").is_empty();
    if !signed_in {
        return None;
    }
    let machine = Machine::here(ctx, signed_in);
    [registered(signed_in, &machine)].into_iter().find(|c| c.status == Status::Fail).map(|c| c.said())
}

#[cfg(feature = "harness")]
fn chose_local(ctx: &Ctx) -> bool {
    super::first_run::chose_local(ctx)
}

#[cfg(not(feature = "harness"))]
fn chose_local(_: &Ctx) -> bool {
    false
}

#[cfg(all(feature = "harness", unix))]
fn service_installed(ctx: &Ctx) -> Option<bool> {
    use krowk_harness::daemon::service;
    Some(service::path(service::Platform::here(), ctx.io.env).is_ok_and(|p| p.is_file()))
}

#[cfg(not(all(feature = "harness", unix)))]
fn service_installed(_: &Ctx) -> Option<bool> {
    None
}

impl Machine {
    /// Read from the keys this machine keeps — never the network.
    #[cfg(feature = "harness")]
    pub(super) fn here(ctx: &Ctx, signed_in: bool) -> Machine {
        use krowk_client::device_chain::Kind;
        if !signed_in {
            return Machine::NotApplicable("no account");
        }
        if !ctx.env("KROWK_TOKEN").is_empty() {
            return Machine::NotApplicable("This machine uses the key in KROWK_TOKEN, which does not register it for sync");
        }
        let kept = (|| {
            let ks = super::sync::keystore(ctx).map_err(|e| e.fix())?;
            Ok::<_, String>((ks.device_list()?, ks.device()?))
        })();
        match kept {
            Err(e) => Machine::Unreadable(e),
            Ok((None, _)) => Machine::NotRegistered,
            Ok((Some(_), None)) => Machine::NotRegistered,
            Ok((Some(chain), Some(me))) => match chain.devices().iter().find(|d| d.id() == me.id()) {
                None => Machine::Removed,
                Some(d) => Machine::Registered {
                    name: super::machine::printable(&d.name),
                    machines: chain.devices().iter().filter(|d| d.kind == Kind::Device && !matches!(d.os.to_ascii_lowercase().as_str(), "ios" | "ipados" | "android")).count(),
                    kit: chain.recovery().is_some(),
                },
            },
        }
    }

    #[cfg(not(feature = "harness"))]
    pub(super) fn here(_: &Ctx, _: bool) -> Machine {
        Machine::NotApplicable("This build carries no sync")
    }
}

/// This directory's repository against the trusted list, by the trust
/// module's own lookup.
#[cfg(feature = "harness")]
pub(super) fn trust_here(ctx: &Ctx) -> Trust {
    use krowk_harness::trust;
    let Ok(cwd) = std::env::current_dir() else { return Trust::NotRepository };
    let root = trust::root(&cwd);
    let home = Some(ctx.env("HOME")).filter(|h| !h.is_empty()).map(std::path::PathBuf::from);
    let Ok(dir) = krowk_api::home::dir(ctx.io.env) else { return Trust::NotRepository };
    let store = trust::Store::new(Some(dir.join(trust::FILE)), home);
    // The home directory and `/` are never trusted, and a directory with no
    // `.git` is no repository.
    if store.refuses(&root).is_some() || root.join(".git").symlink_metadata().is_err() {
        return Trust::NotRepository;
    }
    let shown = krowk_tui::home_relative(&root);
    if store.trusts(&root) { Trust::Trusted(shown) } else { Trust::Untrusted(shown) }
}

#[cfg(not(feature = "harness"))]
pub(super) fn trust_here(_: &Ctx) -> Trust {
    Trust::NotRepository
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_machine(name: &str, machines: usize, kit: bool) -> Machine {
        Machine::Registered { name: name.into(), machines, kit }
    }

    fn assert_fixed_by_a_visible_command(c: &Check) {
        let fix = c.fix.as_deref().unwrap_or_else(|| panic!("{c:?} names no fix"));
        for hidden in ["host enable", "sync init", "devices", "krowk hosts"] {
            assert!(!fix.contains(hidden), "{fix} names the hidden `{hidden}`");
        }
        assert!(fix.contains("krowk"), "{fix}");
    }

    #[test]
    fn the_account_line_passes_signed_in_and_fails_signed_out_with_krowk_login() {
        let ok = Lookup::Ok { email: "you@example.com".into(), workspace: "ws_1".into() };
        let pass = account(true, false, &ok);
        assert_eq!((pass.status, pass.message.as_str(), pass.fix), (Status::Pass, "Signed in as you@example.com", None));
        let fail = account(false, false, &Lookup::Unchecked);
        assert_eq!((fail.status, fail.fix.as_deref()), (Status::Fail, Some("krowk login")));
        assert_fixed_by_a_visible_command(&fail);
        let rejected = account(true, false, &Lookup::Rejected("unauthorized".into()));
        assert_eq!((rejected.status, rejected.fix.as_deref()), (Status::Fail, Some("krowk login")));
        // Offline, a key is neither good nor bad; and "locally" is a choice, not a fault.
        assert_eq!(account(true, false, &Lookup::Offline).status, Status::Pass);
        assert_eq!(account(false, true, &Lookup::Unchecked).status, Status::Skip);
    }

    #[test]
    fn the_machine_line_passes_when_registered_and_fails_otherwise_with_krowk_login() {
        let pass = registered(true, &registered_machine("desk", 1, false));
        assert_eq!((pass.status, pass.message.as_str()), (Status::Pass, "This machine is registered as 'desk'"));
        for m in [Machine::NotRegistered, Machine::Removed, Machine::Unreadable("denied".into())] {
            let c = registered(true, &m);
            assert_eq!(c.status, Status::Fail, "{m:?}");
            assert_fixed_by_a_visible_command(&c);
        }
        assert_eq!(registered(false, &Machine::NotApplicable("no account")).status, Status::Skip);
        assert_eq!(registered(true, &Machine::NotApplicable("a key")).status, Status::Skip);
    }

    #[test]
    fn the_always_on_line_passes_with_the_service_and_fails_without_it() {
        assert_eq!(always_on(true, Some(true)).status, Status::Pass);
        let off = always_on(true, Some(false));
        assert_eq!((off.status, off.fix.as_deref(), off.message.as_str()), (Status::Fail, Some("krowk machines always-on on"), "Always-on service off"));
        assert_fixed_by_a_visible_command(&off);
        assert_eq!(always_on(false, Some(false)).status, Status::Skip);
        assert_eq!(always_on(true, None).status, Status::Skip);
    }

    #[test]
    fn the_trust_line_passes_a_trusted_repository_and_fails_an_untrusted_one() {
        assert_eq!(trusted(&Trust::Trusted("~/r".into())).status, Status::Pass);
        let c = trusted(&Trust::Untrusted("~/r".into()));
        assert_eq!(c.status, Status::Fail);
        assert_fixed_by_a_visible_command(&c);
        assert_eq!(trusted(&Trust::NotRepository).status, Status::Skip);
    }

    #[test]
    fn the_kit_line_fails_only_for_fewer_than_two_machines_and_no_kit() {
        let one = recovery_kit(&registered_machine("a", 1, false));
        assert_eq!((one.status, one.fix.as_deref()), (Status::Fail, Some("krowk machines recovery")));
        assert_eq!(one.message, "One machine, no recovery kit: losing it loses your sessions");
        assert_fixed_by_a_visible_command(&one);
        assert_eq!(recovery_kit(&registered_machine("a", 2, false)).status, Status::Pass);
        assert_eq!(recovery_kit(&registered_machine("a", 1, true)).status, Status::Pass);
        assert_eq!(recovery_kit(&Machine::NotRegistered).status, Status::Skip);
    }

    #[test]
    fn the_plan_line_warns_over_the_limit_with_krowk_billing() {
        let pass = plan(Some("free"), &registered_machine("a", 2, true));
        assert_eq!((pass.status, pass.message.as_str()), (Status::Pass, "Free plan: 2 of 2 machines"));
        let over = plan(Some("free"), &registered_machine("a", 3, true));
        assert_eq!((over.status, over.fix.as_deref()), (Status::Warn, Some("krowk billing")));
        assert_fixed_by_a_visible_command(&over);
        assert_eq!(plan(Some("business"), &registered_machine("a", 40, true)).status, Status::Pass);
        assert_eq!(plan(Some("pro"), &registered_machine("a", 11, true)).status, Status::Warn);
        assert_eq!(plan(None, &Machine::NotRegistered).status, Status::Skip);
    }
}
