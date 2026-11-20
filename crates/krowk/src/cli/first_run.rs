//! The first machine, step 1: signing in to krowk before the TUI opens. A
//! machine with no way to run a model and no krowk account is asked once —
//! sign in in the browser, or use krowk locally. Signing in leaves a
//! workspace key and this machine registered for sync, with no further
//! prompt. The model step is the TUI's own first-run card, which follows.
//!
//! Each step can be cancelled and resumed by running `krowk` again, so what
//! is left to do is read from what is on disk: the key, the device list, and
//! `first-run.json` — `local` once the person chose to use krowk without an
//! account (never offered again), `pending` between signing in and the
//! registration that finishes it. `krowk login` on a terminal runs the same
//! step from the top.

use super::recovery::{register_first_device, Registered};
use super::{agent, auth, Ctx};
use krowk_api::{creds, Client, Error, LoginAction};
use krowk_tui::card::{pick, Pressed};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Chosen: use krowk locally, without an account.
    local: bool,
    /// Signed in, and not yet registered or handed to the add-a-machine flow.
    pending: bool,
}

fn path(ctx: &Ctx) -> Result<PathBuf, Error> {
    Ok(krowk_api::home::dir(ctx.io.env)?.join("first-run.json"))
}

fn load(ctx: &Ctx) -> State {
    path(ctx).ok().and_then(|p| creds::read(&p).ok()).unwrap_or_default()
}

fn save(ctx: &mut Ctx, s: &State) {
    if let Err(e) = path(ctx).and_then(|p| creds::write(&p, s).map_err(|e| krowk_api::fail("first_run_unwritable", e))) {
        ctx.warn(&format!("could not keep where setup is: {}", e.fix()));
    }
}

/// Whether the TUI goes on to open.
pub(super) enum Gate {
    Open,
    Stop,
}

/// Step 1, when nothing here can run a model yet (`runnable` false): asked
/// before the TUI takes the terminal, as the trust question is. A failure
/// is said and left for the next `krowk` to retry — it never keeps the TUI
/// from opening, which still connects a model.
pub(super) fn account_step(ctx: &mut Ctx, runnable: bool) -> Gate {
    if runnable {
        return Gate::Open;
    }
    let mut state = load(ctx);
    let Ok(client) = agent::new_client(ctx) else { return Gate::Open };
    if client.authenticated() {
        if state.pending {
            finish(ctx, client, &mut state);
        }
        return Gate::Open;
    }
    if state.local {
        return Gate::Open;
    }
    match pick("Sign in to krowk", &[("Enter", "open the browser"), ("l", "use krowk locally, without an account")]) {
        Pressed::Key('l') => {
            state.local = true;
            save(ctx, &state);
            Gate::Open
        }
        Pressed::Enter => {
            sign_in(ctx, &mut state);
            Gate::Open
        }
        _ => {
            let _ = writeln!(ctx.io.stderr, "Run krowk again to pick up here.");
            Gate::Stop
        }
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
    finish(ctx, Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), token), &mut state);
}

fn sign_in(ctx: &mut Ctx, state: &mut State) {
    match auth::browser_login(ctx, LoginAction::Login) {
        Ok((token, _)) => {
            state.pending = true;
            save(ctx, state);
            finish(ctx, Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), &token), state);
        }
        Err(e) => ctx.warn(&format!("could not sign in: {} — run krowk again to retry", e.fix())),
    }
}

/// Signed in: this machine registered for sync, or — when the account has
/// machines already — handed to the add-a-machine flow.
fn finish(ctx: &mut Ctx, client: Client, state: &mut State) {
    match register_first_device(ctx, &client) {
        Ok(Registered::New { name }) => {
            let _ = writeln!(ctx.io.stderr, "Signed in. '{name}' is set up to sync.");
            state.pending = false;
            save(ctx, state);
        }
        Ok(Registered::Already) => {
            state.pending = false;
            save(ctx, state);
        }
        Ok(Registered::OthersExist) => add_a_machine(ctx, &client),
        Err(e) => ctx.warn(&format!("signed in, but this machine is not set up to sync yet: {} — run krowk again to retry", e.fix())),
    }
}

/// The hand-off for an account that already has machines: this one joins
/// them rather than starting a device list. `pending` stays set, so
/// `krowk` comes back here until the flow finishes the job.
///
/// TICKET 04 fills this in: ask the registry for a pairing
/// (`Client::request_pairing`), wait for the code, finish joining.
pub(super) fn add_a_machine(ctx: &mut Ctx, _client: &Client) {
    let _ = writeln!(ctx.io.stderr, "Your account already has machines. Adding this one to them is not available yet.");
}
