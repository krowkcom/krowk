//! The first machine's setup, from bare `krowk` on a terminal (D4, D5), as
//! plain prompts on stderr answered with a line on stdin: sign in to krowk
//! (or use it locally), connect a model with `krowk connect`'s own flow, keep
//! this machine available while you're away, and the recovery kit. Signing
//! in leaves a workspace key and this machine registered for sync, with no
//! further prompt; an account that already has machines adds this one to
//! them with a code instead. Without a terminal nothing is asked.
//!
//! Each step can be cancelled and resumed by running `krowk` again, so what
//! is left to do is read from what is on disk: the key, the device list, and
//! `first-run.json` — `local` once the person chose to use krowk without an
//! account (never offered again), `pending` between signing in and the
//! registration that finishes it, and the two questions once asked. The
//! file is there once the setup has begun. `krowk login` on a terminal runs
//! the account step from the top, and the next `krowk` goes on from there.

use super::recovery::{self, register_first_device, Registered};
use super::{agent, auth, Ctx};
use gruntas_api::{creds, Client, Error, LoginAction};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct State {
    /// Chosen: use krowk locally, without an account.
    local: bool,
    /// Signed in, and not yet registered or handed to the add-a-machine flow.
    pending: bool,
    /// The always-on question was asked, and answered or left unanswered at
    /// the end of input: not asked again.
    always_on_asked: bool,
    /// The recovery kit was offered once, made or put off: not offered again
    /// (`krowk machines recovery` makes one, and the reminder stays).
    kit_offered: bool,
}

fn path(ctx: &Ctx) -> Result<PathBuf, Error> {
    Ok(gruntas_api::home::dir(ctx.io.env)?.join("first-run.json"))
}

fn load(ctx: &Ctx) -> State {
    path(ctx).ok().and_then(|p| creds::read(&p).ok()).unwrap_or_default()
}

fn save(ctx: &mut Ctx, s: &State) {
    if let Err(e) = path(ctx).and_then(|p| creds::write(&p, s).map_err(|e| gruntas_api::fail("first_run_unwritable", e))) {
        ctx.warn(&format!("could not keep where setup is: {}", e.fix()));
    }
}

/// Whether the person chose to use krowk locally, without an account.
pub(super) fn chose_local(ctx: &Ctx) -> bool {
    load(ctx).local
}

/// Bare `krowk` with a person at the terminal: the setup's steps still to
/// do, in order, before the overview. The sign-in step is asked only when
/// nothing here can run a model yet and there is no key; the rest only once
/// the setup has begun (`first-run.json` is there), the account step is
/// behind it (signed in, or using krowk locally), and until both questions
/// are asked. A step cancelled or failed is said, and the next `krowk`
/// takes it up again.
pub(super) fn bare(ctx: &mut Ctx) {
    account_step(ctx, runnable(ctx));
    // A key in the environment is the caller's, not this machine's account.
    let past_account = load(ctx).local || (ctx.env("KROWK_TOKEN").is_empty() && agent::new_client(ctx).is_ok_and(|c| c.authenticated()));
    if !path(ctx).is_ok_and(|p| p.exists()) || !past_account || !steps_due(ctx) {
        return;
    }
    if model_step(ctx) {
        after_model_step(ctx);
    }
}

/// What can run a model without asking a vendor: a key, a login krowk
/// keeps, a server. A vendor's own login is not told apart from none, so a
/// machine on one with no krowk account is asked to sign in once.
fn runnable(ctx: &Ctx) -> bool {
    use gruntas_harness::readiness::{self, Readiness};
    let (Ok(config), Ok(credentials)) = (super::prompt::load_instances(), super::providers::credentials_path()) else { return true };
    let registry = gruntas_harness::instances::Registry::resolve(&config, ctx.io.env);
    registry.instances.values().filter_map(|i| readiness::local(i, &credentials)).any(|r| matches!(r, Readiness::Ready { .. } | Readiness::Unknown { .. }))
}

/// Step 2: a model, connected with `krowk connect`'s own pickers when none
/// is ready here, vendors asked too. Whether the setup goes on.
fn model_step(ctx: &mut Ctx) -> bool {
    let ready = super::status::reports(ctx).is_ok_and(|(_, _, r)| r.iter().any(|r| r.readiness.is_ready()));
    if ready {
        return true;
    }
    let _ = writeln!(ctx.io.stderr, "\nConnect a model");
    match super::providers::connect(ctx, &[]) {
        Ok(()) => true,
        Err(e) => {
            ctx.warn(&format!("no model is connected: {} — run krowk again to connect one", e.fix()));
            false
        }
    }
}

/// The lines bare `krowk` puts under its overview: the most important
/// problem with this setup and its fix (D9), and the recovery-kit reminder
/// while the person has fewer than two machines (D5). From the disk alone.
pub(super) fn overview_lines(ctx: &Ctx) -> Vec<String> {
    super::health::top_problem(ctx).into_iter().chain(recovery::reminder_due_here(ctx).then(|| recovery::REMINDER.to_string())).collect()
}

/// Step 1: sign in to krowk, or use it locally. A registration left
/// unfinished is taken up whenever bare `krowk` runs on a terminal; the
/// question is asked only when nothing here can run a model yet
/// (`runnable` false) and there is no key. A failure is said and left for
/// the next `krowk` to retry, and any other answer is "not now": asked
/// again next time.
fn account_step(ctx: &mut Ctx, runnable: bool) {
    let mut state = load(ctx);
    // A key in the environment is the caller's own, not this machine's
    // account: with it set, the step is not asked and nothing owed is finished.
    if !ctx.env("KROWK_TOKEN").is_empty() {
        return;
    }
    let Ok(client) = agent::new_client(ctx) else { return };
    if client.authenticated() {
        if state.pending {
            let _ = writeln!(ctx.io.stderr, "Finishing setting this machine up to sync…");
            finish(ctx, client, &mut state);
        }
        return;
    }
    if runnable || state.local {
        return;
    }
    match super::chain::ask_key(ctx, "Sign in to krowk", &[("Enter", "open the browser"), ("l", "use krowk locally, without an account")]).as_str() {
        "l" => {
            state.local = true;
            state.pending = false;
            save(ctx, &state);
        }
        "" => sign_in(ctx, &mut state),
        _ => {}
    }
}

/// `krowk login` on a terminal, once the browser has minted `token`: the
/// same step from the top — the machine is no longer "local", and is
/// registered if it is not.
pub(super) fn after_login(ctx: &mut Ctx, token: &str) {
    let mut state = load(ctx);
    state.local = false;
    state.pending = true;
    save(ctx, &state);
    finish(ctx, Client::new(&gruntas_api::base_url_for(ctx.f.dev, ctx.io.env), token), &mut state);
}

fn sign_in(ctx: &mut Ctx, state: &mut State) {
    // Before the key is stored, so a stop between the two leaves the
    // registration owed, never forgotten (`pending` means nothing until
    // there is a key).
    state.pending = true;
    save(ctx, state);
    match auth::browser_login(ctx, LoginAction::Login) {
        Ok((token, _)) => {
            finish(ctx, Client::new(&gruntas_api::base_url_for(ctx.f.dev, ctx.io.env), &token), state);
        }
        // Nothing was stored: no key, nothing owed.
        Err(e) => {
            state.pending = false;
            save(ctx, state);
            ctx.warn(&format!("could not sign in: {} — run krowk again to retry", e.fix()));
        }
    }
}

/// Signed in: this machine registered for sync, or — when the account has
/// machines already — handed to the add-a-machine flow.
fn finish(ctx: &mut Ctx, client: Client, state: &mut State) {
    match register_first_device(ctx, &client) {
        Ok(Registered::New { name }) => {
            state.pending = false;
            save(ctx, state);
            let _ = writeln!(ctx.io.stderr, "Signed in. '{name}' is set up to sync.");
        }
        Ok(Registered::Already) => {
            state.pending = false;
            save(ctx, state);
        }
        Ok(Registered::OthersExist) => add_a_machine(ctx, client, state),
        // A key the registry refuses is not retried on every launch.
        Err(e) if e.status == 401 || e.status == 403 => {
            state.pending = false;
            save(ctx, state);
            ctx.warn(&format!("this machine is not set up to sync: {} — `krowk login` signs in again", e.fix()));
        }
        Err(e) => ctx.warn(&format!("signed in, but this machine is not set up to sync yet: {} — run krowk again to retry", e.fix())),
    }
}

/// The hand-off for an account that already has machines: this one joins
/// them rather than starting a device list.
///
/// The new machine asks the person's others to show a code, takes it at a
/// prompt, and joins (`pairing::join_new_machine`: R-EASE-5, D6). `pending`
/// stays set until the machine has joined, so a cancelled or failed one —
/// no machine online, a wrong code — resumes at the next `krowk`; the model
/// step follows either way.
pub(super) fn add_a_machine(ctx: &mut Ctx, client: Client, state: &mut State) {
    let _ = writeln!(ctx.io.stderr, "Your account already has machines. Adding this one to them takes a code from one of them.");
    match super::pairing::join_new_machine(ctx, client) {
        Ok(()) => {
            state.pending = false;
            save(ctx, state);
        }
        // Already on the list: nothing is owed.
        Err(e) if e.code() == "already_joined" => {
            state.pending = false;
            save(ctx, state);
        }
        Err(e) => ctx.warn(&format!("this machine is not added to your others yet: {} — run krowk again to retry", e.fix())),
    }
}

/// Whether `after_model_step` has anything to ask.
fn steps_due(ctx: &Ctx) -> bool {
    let state = load(ctx);
    !state.always_on_asked || (!state.kit_offered && recovery::reminder_due_here(ctx))
}

/// The questions after the model step (D4, D5), on the terminal, each asked
/// once: keep this machine available, then the recovery kit. Cancelled or
/// failed, a step is said and not retried every launch; the kit's reminder
/// stays in the overview while it is due.
fn after_model_step(ctx: &mut Ctx) {
    let mut state = load(ctx);
    // Marked once answered, not before: Ctrl-C at the question leaves it to be
    // asked next time.
    if !state.always_on_asked {
        always_on(ctx);
        state.always_on_asked = true;
        save(ctx, &state);
    }
    // Only a machine that is on a device list has a kit to offer on it: one
    // using krowk locally is offered it the day it signs in, by the reminder.
    if !state.kit_offered && recovery::reminder_due_here(ctx) {
        if let Err(e) = recovery::offer_kit(ctx) {
            ctx.warn(&format!("no recovery kit was made: {} — `krowk machines recovery` makes one", e.fix()));
        }
        state.kit_offered = true;
        save(ctx, &state);
    }
}

/// Whether the always-on step defaults to yes: on a desktop, not on a
/// laptop (R-EASE-3).
#[cfg(unix)]
fn always_on(ctx: &mut Ctx) {
    let default_yes = !super::power::has_battery();
    let title = format!("Keep this machine available while you're away?\nIt syncs your sessions and takes work from your other machines. {}", if default_yes { "[Y/n]" } else { "[y/N]" });
    let yes = match super::chain::ask_key(ctx, &title, &[]).as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    };
    if !yes {
        let _ = writeln!(ctx.io.stderr, "Not enabled. `krowk machines always-on on` does it later.");
        return;
    }
    match super::host::set_service(ctx, true) {
        Ok((file, _)) => {
            let _ = writeln!(ctx.io.stderr, "This machine stays available: the host runs as a service ({}).", file.display());
        }
        Err(e) => ctx.warn(&format!("the always-on service did not start: {} — `krowk machines always-on on` tries again", e.fix())),
    }
}

/// No service to keep a host in on this platform: nothing to ask.
#[cfg(not(unix))]
fn always_on(_: &mut Ctx) {}
