//! `krowk sync init`, `status`, `recover` and `recovery new|check`: a
//! person's device list begun, checked, got back into, and its recovery kit
//! replaced (canon, engineering/devices.md → Recovery, Replacing the kit,
//! Starting over).
//!
//! - `init` makes seq 0: this device and, unless
//!   the person skips it, the recovery device the kit derives, each signing
//!   the entry, with generation 1 of the user key wrapped to both. The kit's
//!   12 words go to stderr ([Enter] done, [s] skip) or to `--save FILE`,
//!   0600 — never stdout, `--json` or a log. There is no typing them back.
//!   On a person who already has a list, `--start-over` makes a new one;
//!   every other device on the old one stops and asks to be paired again.
//!   Run from a device that holds the user key, the sessions it can open
//!   come along, sealed again under the new list (`reseal`).
//! - `status` checks the list against the one kept here, takes up a newer
//!   user key, and says what is missing: a kit, above all.
//! - `recover`, on a new machine: the words derive the recovery device, the
//!   list is verified from seq 0, and the person goes through every device
//!   on it to keep or remove. Nothing is wrapped to anyone until they have:
//!   the one post removes the rest, adds this machine, and wraps the newest
//!   key only to what was kept.
//! - `recovery new` replaces the kit at once, with the old kit's words —
//!   both kits sign, and the key rotates — or from any device when there is
//!   no kit. A lost kit with working devices is a start-over.
//!   `recovery check` tests the words against the list.

use super::chain::{self, ask, ask_line, ask_words, describe, is_kit, kit_subject, need_person, now, post_of, say, show_kit, this_subject, Me};
use super::reseal::{self, Old};
use super::Ctx;
use krowk_api::{fail, Client, Error, LoginAction};
use krowk_client::device_chain::{Batch, Change, Chain, Device, Kind, SignedEntry, Subject};
use krowk_client::recovery::{RecoveryDevice, RecoveryKit};
use krowk_client::user_key::UserKeys;
use serde_json::json;

/// What skipping the kit risks, said where the person decides and on every
/// device until there is one. A second device is no backup: it's another
/// thing to lose.
pub(super) const NO_KIT: &str = "no recovery kit: if you lose every device you lose every session, and a stolen device that removes the others first can lock you out — `krowk sync recovery new` makes one";

/// What to do with a lost kit and working devices: start over from one of
/// them, which brings their sessions along.
const LOST_KIT: &str = "a kit on your list is replaced only with its words; if they are lost, start over from this device with `krowk sync init --start-over`, which makes a new kit and keeps the sessions this device can open";

/// A kit, shown or saved. `false` when the person skipped it.
fn hand_over_kit(ctx: &mut Ctx, kit: &RecoveryKit, skippable: bool) -> Result<bool, Error> {
    let words = kit.words();
    if !ctx.f.save.is_empty() {
        let path = ctx.f.save.clone();
        chain::save_kit(&path, &words)?;
        let mode = if cfg!(unix) { " (0600)" } else { "" };
        let _ = writeln!(ctx.io.stderr, "Your recovery kit is in {path}{mode}: the only way back in if you lose every device. Keep a copy off this machine, and delete this one when you have.");
        return Ok(true);
    }
    show_kit(ctx, &words);
    drop(words);
    let prompt = if skippable { "[Enter] done   [s] skip for now" } else { "[Enter] done" };
    let answer = ask_line(ctx, prompt)?;
    if skippable && answer.trim().eq_ignore_ascii_case("s") {
        let _ = writeln!(ctx.io.stderr, "Skipped: {NO_KIT}.");
        return Ok(false);
    }
    Ok(true)
}

/// A post that failed after the kit was handed over: its words are on no
/// list, so the person is told to throw them away, and a `--save` file goes.
fn discard_kit(ctx: &mut Ctx, handed: bool) {
    if !handed {
        return;
    }
    if !ctx.f.save.is_empty() {
        let _ = std::fs::remove_file(&ctx.f.save);
    }
    let _ = writeln!(ctx.io.stderr, "That recovery kit is not on your list — discard it; nothing was changed.");
}

/// Refused before the sign-in, which would replace this device's key with
/// one that speaks for no device: a machine that keeps a list, or holds
/// user keys, is set up.
fn refuse_if_set_up(ctx: &Ctx, why: &str) -> Result<(), Error> {
    let store = super::sync::keystore(ctx)?;
    if store.device_list_path().exists() || store.user_keys_path().exists() {
        return Err(fail("already_set_up", why.to_string()));
    }
    Ok(())
}

/// The old list and the user keys that open it, held here — opened before
/// anything changes, so a start-over can bring their sessions along.
fn held_old(ctx: &Ctx) -> Result<Option<Old>, Error> {
    let chain = super::sync::keystore(ctx)?.device_list().map_err(|e| fail("keys_unreadable", e))?;
    let keys = chain::held_keys(ctx)?;
    Ok(chain.zip(keys).map(|(chain, keys)| Old { chain, keys }))
}

fn confirm_start_over(ctx: &mut Ctx, carries: bool) -> Result<(), Error> {
    let sessions = match carries {
        true => "The sessions this device can open come along: they are sealed again under the new list.",
        false => "Nothing synced so far can be opened again: no device here holds its keys.",
    };
    let _ = writeln!(ctx.io.stderr, "Starting over replaces your device list. Every other device on it stops syncing until it is paired again. {sessions}");
    match ask(ctx, "Start over with a new device list?")? {
        true => Ok(()),
        false => Err(fail("selection_cancelled", "not confirmed, so nothing was changed")),
    }
}

/// Posts seq 0. The client that posted comes back: init binds its key to
/// this device.
fn post_init(client: Client, me: &Me, batch: &Batch, start_over: bool) -> Result<Client, Error> {
    let post = krowk_api::sync::ListPost { start_over, ..post_of(batch) };
    me.sign(&client).init_device_list(&post).map(|_| client)
}

pub(super) fn init(ctx: &mut Ctx) -> Result<(), Error> {
    need_person(ctx, "`krowk sync init` shows your recovery kit", false)?;
    let home = chain::home(ctx)?;
    if !ctx.f.start_over {
        refuse_if_set_up(ctx, "sync is set up on this machine already — `krowk sync status` shows its device list. Only if every device and the kit are lost, or the kit alone: `krowk sync init --start-over`")?;
    }
    let old = if ctx.f.start_over { held_old(ctx)? } else { None };
    let action = if ctx.f.start_over { LoginAction::StartOver } else { LoginAction::Login };
    let (client, served) = chain::signed_in_list(ctx, action)?;
    if let Some(aside) = reseal::set_aside(&home)?.filter(|a| ctx.f.start_over && served.first().is_some_and(|e| e.hash() != a.chain.root())) {
        return finish_start_over(ctx, client, aside, served);
    }
    if !served.is_empty() && !ctx.f.start_over {
        return Err(fail("chain_exists", "you already have a device list — add this machine from one of your devices with `krowk sync join`, or back in with the kit's words with `krowk sync recover`. Only if every device and the kit are lost: `krowk sync init --start-over`"));
    }
    let start_over = ctx.f.start_over && !served.is_empty();
    if start_over {
        confirm_start_over(ctx, old.is_some())?;
    }
    let me = Me::load(ctx)?;
    let kit = RecoveryKit::generate();
    let kept = hand_over_kit(ctx, &kit, true)?;
    let recovery = kept.then(|| kit.device());
    drop(kit);
    let first = this_subject(ctx, &me.device, &me.signing);
    let (new_chain, batch) = Chain::start(first.clone(), &me.signing, recovery.as_ref().map(|r| (kit_subject(r), &r.signing)), now()).map_err(|e| fail("sync_setup_failed", e.0))?;
    if old.is_some() {
        reseal::copy_aside(&home)?;
    }
    let client = post_init(client, &me, &batch, start_over).inspect_err(|_| discard_kit(ctx, kept))?;
    let keys = keep_new(ctx, &new_chain, &batch)?;
    let count = match old.filter(|_| start_over) {
        Some(old) => Some(reseal::run(ctx, &client, &me, &old, &keys, &new_chain).map_err(unfinished)?),
        None => None,
    };
    let name = first.name.clone();
    let summary = init_summary(&name, start_over, kept, count.as_ref());
    say(ctx, json!({ "device": me.device.id().to_string(), "name": name, "generation": 1, "recovery_kit": kept, "started_over": start_over, "resealed": count.as_ref().map(|c| c.sealed), "left": count.as_ref().map(|c| c.left) }), summary)
}

/// Generation 1 and the new list, kept in place of the old ones — before
/// any re-seal, so a device cut short in one still syncs.
fn keep_new(ctx: &Ctx, new: &Chain, batch: &Batch) -> Result<UserKeys, Error> {
    let store = super::sync::keystore(ctx)?;
    store.forget_user_keys().map_err(|e| fail("sync_setup_failed", e))?;
    let keys = chain::save_keys(ctx, UserKeys::new(batch.newest.clone(), Vec::new()).map_err(|e| fail("sync_setup_failed", e.0))?, new)?;
    keep_list(ctx, &batch.entries)?;
    Ok(keys)
}

fn keep_list(ctx: &Ctx, entries: &[SignedEntry]) -> Result<(), Error> {
    super::sync::keystore(ctx)?.save_device_list(entries).map_err(|e| fail("sync_setup_failed", e)).map(|_| ())
}

/// A re-seal the network or the registry cut short: the device syncs, and
/// running the start-over again goes on from where this stopped.
fn unfinished(e: Error) -> Error {
    fail(&e.code(), format!("{} — the new list is in place and this device syncs; run `krowk sync init --start-over` again to go on sealing your old sessions under it", e.fix()))
}

/// A start-over run again, the new list already posted: this device on it
/// with generation 1 (taken up now if a crash came before it was kept), and
/// what the old keys open sealed again under it.
fn finish_start_over(ctx: &mut Ctx, client: Client, old: Old, served: Vec<SignedEntry>) -> Result<(), Error> {
    let me = Me::load(ctx)?;
    let new = Chain::verify(&served, None).map_err(|e| fail("device_list_refused", format!("{} — nothing was changed", e.0)))?;
    if !new.devices().iter().any(|d| d.id() == me.device.id()) {
        return Err(fail("device_removed", "this device is not on the list the registry serves — it was not the one that started over"));
    }
    let store = super::sync::keystore(ctx)?;
    if store.device_list().map_err(|e| fail("keys_unreadable", e))?.map(|c| c.root()) != Some(new.root()) {
        store.forget_user_keys().map_err(|e| fail("sync_setup_failed", e))?;
        keep_list(ctx, &served)?;
    }
    let keys = chain::adopt_user_key(ctx, &client, &me, &new)?;
    let count = reseal::run(ctx, &client, &me, &old, &keys, &new).map_err(unfinished)?;
    say(ctx, json!({ "resealed": count.sealed, "already": count.already, "left": count.left }), resealed(&count))
}

fn resealed(c: &reseal::Count) -> String {
    let left = match c.left {
        0 => String::new(),
        _ => " — run `krowk sync init --start-over` again under that workspace's key to finish, and `krowk sync recovery discard-old` once none is left".to_string(),
    };
    format!("{} re-sealed, {} left (other workspaces or refused){left}", c.sealed, c.left)
}

/// What init did. A skipped kit is not said again here: the person was told
/// what skipping risks when they pressed [s], a line above.
fn init_summary(name: &str, start_over: bool, kept: bool, count: Option<&reseal::Count>) -> String {
    let sessions = count.map(|c| format!("; old sessions: {}", resealed(c))).unwrap_or_default();
    let with_kit = if kept { " and your recovery kit" } else { "" };
    match (start_over, kept) {
        (true, _) => format!("started over: a new device list with '{name}'{with_kit}; pair your other devices again with `krowk sync join`{sessions}"),
        (false, true) => format!("sync is set up: '{name}' is your first device, and your recovery kit is on the list"),
        (false, false) => format!("sync is set up: '{name}' is your first device"),
    }
}

/// `krowk sync recovery discard-old`: drops the old keys a start-over kept
/// aside, once the person says so — after being told how many sessions in
/// this workspace only they still open.
pub(super) fn discard_old(ctx: &mut Ctx) -> Result<(), Error> {
    need_person(ctx, "`krowk sync recovery discard-old` throws away the keys that open your old sessions", false)?;
    let home = chain::home(ctx)?;
    let Some(old) = reseal::set_aside(&home)? else {
        return say(ctx, json!({ "discarded": false }), "nothing is set aside here — no start-over left old keys behind".into());
    };
    let me = Me::load(ctx)?;
    let client = super::sync::keyed_client(ctx, "`krowk sync recovery discard-old`")?;
    let v = chain::verified(ctx, &client, &me)?;
    let keys = chain::held_keys(ctx)?.ok_or_else(chain::not_set_up)?;
    let left = reseal::still_left(ctx, &client, &me, &old, &keys, &v.chain)?;
    let _ = writeln!(ctx.io.stderr, "{left} session(s) in this workspace open only with the old keys, and are lost with them; sessions in your other workspaces are not counted here.");
    if !ask(ctx, "Discard the old keys?")? {
        return Err(fail("selection_cancelled", "not confirmed, so the old keys are kept"));
    }
    std::fs::remove_dir_all(reseal::aside_dir(&home)).map_err(|e| fail("sync_setup_failed", format!("the old keys could not be removed: {e}")))?;
    say(ctx, json!({ "discarded": true, "left": left }), format!("the old keys are gone; {left} session(s) here could only be opened with them"))
}

pub(super) fn status(ctx: &mut Ctx) -> Result<(), Error> {
    let kept = super::sync::keystore(ctx)?.device_list().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(chain::not_set_up)?;
    let me = Me::load(ctx)?;
    let client = super::sync::keyed_client(ctx, "`krowk sync status`")?;
    let v = match chain::verified(ctx, &client, &me) {
        Ok(v) => v,
        // Only the network or the registry's own failure reads as offline:
        // every refusal this device makes itself — a reset list, a forked
        // one, a removal — must stop here.
        Err(e) if e.code() == "network_unreachable" || e.status >= 500 => {
            let kit = if kept.recovery().is_some() { "recovery kit on the list" } else { NO_KIT };
            let summary = format!("not checked with the registry ({}); as last verified: device list at entry {}, {kit}", e.code(), kept.head().seq);
            return say(ctx, json!({ "checked": false, "seq": kept.head().seq, "recovery_kit": kept.recovery().is_some() }), summary);
        }
        Err(e) => return Err(e),
    };
    let listed = chain::listed_devices(ctx, &client);
    let revoked = chain::revoked_on_dashboard(&v.chain, &listed);
    let devices: Vec<_> = v.chain.devices().iter().map(|d| json!({ "id": d.id().to_string(), "kind": if d.kind == Kind::Recovery { "recovery" } else { "device" }, "name": d.name, "os": d.os, "this_device": d.id() == me.device.id(), "revoked": revoked.iter().any(|r| r.id() == d.id()) })).collect();
    let data = json!({ "checked": true, "seq": v.chain.head().seq, "generation": v.chain.generation(), "devices": devices, "recovery_kit": v.chain.recovery().is_some() });
    let summary = status_lines(&v, &me, &revoked).join("\n");
    say(ctx, data, summary)?;
    // A dashboard Revoke is finished from a device: the web can't sign the
    // list. Asked only of a person at a terminal reading the human format —
    // after `--json` the document stays the one thing on stdout — and
    // everyone else was told.
    let attended = ctx.format == crate::output::Format::Human && chain::attended(ctx);
    let revoked: Vec<_> = revoked.into_iter().filter(|d| d.id() != me.device.id()).collect();
    if attended && !revoked.is_empty() && chain::ask(ctx, "Finish removing the device(s) revoked on the dashboard now?")? {
        return super::devices::finish(ctx, &me, &v, &revoked);
    }
    Ok(())
}

fn status_lines(v: &chain::Verified, me: &Me, revoked: &[&Device]) -> Vec<String> {
    let mut lines = vec![format!("device list at entry {}, user key generation {}", v.chain.head().seq, v.chain.generation())];
    for d in v.chain.devices() {
        let this = if d.id() == me.device.id() { "  (this device)" } else { "" };
        lines.push(format!("  {}{this}", describe(d, &v.entries)));
    }
    if v.chain.recovery().is_none() {
        lines.push(NO_KIT.to_string());
    }
    for r in revoked {
        let name = super::sync::printable(&r.name);
        lines.push(format!("'{name}' was revoked on the dashboard but is still on your device list — finish removing it with `krowk devices remove '{name}'`, which rotates your key away from it"));
    }
    lines
}

pub(super) fn check(ctx: &mut Ctx) -> Result<(), Error> {
    let kept = super::sync::keystore(ctx)?.device_list().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(chain::not_set_up)?;
    let kit = read_kit(ctx, "Recovery kit (12 words):")?.ok_or_else(|| fail("bad_recovery_kit", "no words were entered, so nothing was checked"))?;
    if is_kit(&kept, &kit.device()) {
        return say(ctx, json!({ "matches": true }), "these words are your recovery kit, as your device list last verified here holds it".into());
    }
    let why = match kept.recovery() {
        Some(_) => "these words are not the recovery kit on your device list — check each word against where you wrote it down; a kit replaced since is no longer yours",
        None => "your device list has no recovery kit to check against — `krowk sync recovery new` makes one",
    };
    Err(fail("kit_not_on_list", why))
}

/// The kit's words back to its recovery device, refused with the word, the
/// count or the checksum that is wrong. None when nothing was entered.
fn read_kit(ctx: &mut Ctx, prompt: &str) -> Result<Option<RecoveryKit>, Error> {
    let words = ask_words(ctx, prompt)?;
    if words.trim().is_empty() {
        return Ok(None);
    }
    RecoveryKit::from_words(&words).map(Some).map_err(|e| fail("bad_recovery_kit", e))
}

/// This machine as it would join the list: refused before the review when
/// its key is on the list already, or a device there has its name.
fn joining(ctx: &Ctx, chain: &Chain, me: &Me) -> Result<Subject, Error> {
    if chain.devices().iter().any(|d| d.id() == me.device.id()) {
        return Err(fail("already_on_list", "this machine's device key is already on your list — run `krowk sync status` here instead"));
    }
    let subject = this_subject(ctx, &me.device, &me.signing);
    if chain.devices().iter().any(|d| krowk_client::device_chain::same_name(&d.name, &subject.name)) {
        return Err(fail("device_name_taken", format!("a device on your list is already called '{}' — give this machine another name with --name", subject.name)));
    }
    Ok(subject)
}

/// Every device by name, kept or removed by the person, with nothing
/// wrapped to any of them before it is done. The removals, and the names
/// kept.
fn review(ctx: &mut Ctx, chain: &Chain, entries: &[SignedEntry], revoked: &[&Device]) -> Result<(Vec<Change<'static>>, Vec<String>), Error> {
    let devices: Vec<_> = chain.devices().iter().filter(|d| d.kind == Kind::Device).collect();
    let _ = writeln!(ctx.io.stderr, "Your device list has {} device(s). Keep only the ones you still have — a device you lost, or don't recognise, is removed and gets none of your keys from now on.", devices.len());
    let (mut changes, mut kept) = (Vec::new(), Vec::new());
    for d in devices {
        let note = if revoked.iter().any(|r| r.id() == d.id()) { " — revoked on the dashboard" } else { "" };
        if ask(ctx, &format!("Keep {}{note}?", describe(d, entries)))? {
            kept.push(d.name.clone());
        } else {
            changes.push(Change::Remove(Subject::of(d)));
        }
    }
    Ok((changes, kept))
}

pub(super) fn recover(ctx: &mut Ctx) -> Result<(), Error> {
    let piped = !ctx.io.stdin_tty;
    need_person(ctx, "`krowk sync recover` goes through every device on your list with you", piped)?;
    refuse_if_set_up(ctx, "this machine is already set up for sync — `krowk sync recover` is for a new machine; `krowk devices remove` takes a lost device off from here")?;
    let rdev = read_kit(ctx, "Recovery kit (12 words):")?.ok_or_else(|| fail("bad_recovery_kit", "no words were entered, so nothing was done"))?.device();
    let (client, entries) = chain::signed_in_list(ctx, LoginAction::Login)?;
    if entries.is_empty() {
        return Err(fail("no_device_list", "you have no device list to recover — `krowk sync init` sets sync up"));
    }
    // A new machine keeps no list yet: the whole of it, verified from seq 0.
    let chain = Chain::verify(&entries, None).map_err(|e| fail("device_list_refused", format!("{} — nothing was changed", e.0)))?;
    if !is_kit(&chain, &rdev) {
        return Err(fail("kit_not_on_list", "these words are not the recovery kit on your device list — check each word against where you wrote it down. A kit replaced since, or a list started over, no longer takes them. Nothing was changed"));
    }
    let me = Me::load(ctx)?;
    let subject = joining(ctx, &chain, &me)?;
    let kit_client = chain::signed_as(&client, &rdev.key, &rdev.signing);
    let kit_wraps = kit_client.user_key()?;
    let held = chain::open_for_kit(&kit_wraps, &chain, &rdev)?;
    let listed = chain::listed_devices(ctx, &client);
    let (mut changes, kept) = review(ctx, &chain, &entries, &chain::revoked_on_dashboard(&chain, &listed))?;
    let removed = changes.len();
    changes.push(Change::Add(subject.clone()));
    let (new_chain, batch) = chain.batch(&held, changes, rdev.key.id(), &rdev.signing, now()).map_err(|e| fail("sync_setup_failed", e.0))?;
    kit_client.append_device_list(&post_of(&batch))?;
    // The key speaks for this machine from now on, as init's does.
    chain::claim(&client, &me)?;
    chain::keep(ctx, &entries, &new_chain, &batch, &me, chain::links_of(&kit_wraps))?;
    let summary = match removed {
        0 => format!("back in: '{}' is on your device list, and every device you had is kept", subject.name),
        n => format!("back in: '{}' is on your device list, {n} device(s) removed; new sessions are sealed under key generation {}", subject.name, new_chain.generation()),
    };
    say(ctx, json!({ "device": me.device.id().to_string(), "name": subject.name, "kept": kept, "removed": removed, "generation": new_chain.generation() }), summary)
}

/// With a kit on the list, only the old kit replaces it: a stolen device
/// must not swap in a kit of its own. With none, any device makes one.
fn old_kit(ctx: &mut Ctx, chain: &Chain) -> Result<Option<RecoveryDevice>, Error> {
    if chain.recovery().is_none() {
        return Ok(None);
    }
    let old = read_kit(ctx, "Your current kit's 12 words:")?.ok_or_else(|| fail("kit_required", LOST_KIT))?.device();
    if !is_kit(chain, &old) {
        return Err(fail("kit_not_on_list", format!("these words are not the recovery kit on your device list — nothing was changed. {LOST_KIT}")));
    }
    Ok(Some(old))
}

pub(super) fn new_kit(ctx: &mut Ctx) -> Result<(), Error> {
    need_person(ctx, "`krowk sync recovery new` shows a new recovery kit", false)?;
    let me = Me::load(ctx)?;
    let client = chain::signed_in(ctx, LoginAction::ReplaceKit)?;
    let v = chain::verified(ctx, &client, &me)?;
    let keys = chain::held_keys(ctx)?.ok_or_else(chain::not_set_up)?;
    let old = old_kit(ctx, &v.chain)?;
    let kit = RecoveryKit::generate();
    let nd = kit.device();
    hand_over_kit(ctx, &kit, false)?;
    drop(kit);
    let change = vec![Change::RotateRecovery(kit_subject(&nd), &nd.signing)];
    // Signed by whoever authorizes it — the old kit, else this device —
    // and the request by the same, since the poster must be a signer.
    let (signer, key) = match &old {
        Some(od) => (&od.key, &od.signing),
        None => (&me.device, &me.signing),
    };
    let (next, batch) = v.chain.batch(keys.newest(), change, signer.id(), key, now()).map_err(|e| fail("sync_setup_failed", e.0))?;
    chain::signed_as(&client, signer, key).append_device_list(&post_of(&batch)).inspect_err(|_| discard_kit(ctx, true))?;
    chain::keep(ctx, &v.entries, &next, &batch, &me, chain::older_of(&keys))?;
    let summary = format!("the new recovery kit is in effect; the old one opens nothing sealed from now on (key generation {})", next.generation());
    say(ctx, json!({ "generation": next.generation() }), summary)
}
