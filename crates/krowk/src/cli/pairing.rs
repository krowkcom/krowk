//! Adding a device by a short code (R-E2E-3; canon, engineering/devices.md
//! → Adding a device): `krowk machines add` on a device already on the
//! person's list (A), `krowk sync join` on the new machine (B). The crypto
//! is `krowk_client::pairing` and `device_chain`; this is the command line
//! around them and the registry's mailbox between them.
//!
//! What A seals to B once the person has said yes, built and checked in
//! `krowk_client::pairing_payload`, is the whole signed chain with the new
//! `add` entry on its end, the user key wrapped to B, and A's signing key.
//! B trusts it because the pairing authenticated it, not because the
//! registry served it, and keeps nothing until the chain verifies from seq
//! 0 and its last entry adds exactly B's keys, signed by A. A posts the
//! entry and the wrap once B has acked — and also once the reply has gone
//! out and B never acks: the reply carries the user key, so a machine that
//! may hold it is put on the list, where `krowk machines remove` can take it
//! off and rotate the key away from it, rather than left holding the key
//! where nothing can see it. devices.md states the same rule; a reply
//! that never went out posts nothing.
//!
//! **After any failure on B, the code is gone.** A step that fails, a
//! refusal, a timeout, a dropped connection — B ends the pairing, tells the
//! person to get a new code from A, and never starts again with the typed
//! string: each start gives a registry posing as A one more guess. Nothing
//! is retried on B. A, whose side holds no guess, reads the pairing back
//! after a step whose answer was lost rather than sending the step again,
//! since a step sent twice is out of turn and ends the pairing.
//!
//! B's side, from the hello to the claim, is `krowk_client::pairing_join`,
//! which holds every client to these rules; the CLI supplies its mailbox.
//!
//! What B cannot stop is the person typing the same code into a second
//! `krowk sync join`: A's screen still shows it, and a registry that ended
//! B's first run gets another guess from the second. So B says a code works
//! once and to get a new one from the other device. The typed line itself
//! is wiped once parsed; the line editor's own buffer is not, as crypto.md
//! accepts for the recovery kit.
//!
//! Both commands need a person at a terminal: an agent told to run `krowk
//! devices add` must not hand a stranger's machine every session. Debug
//! builds let the test suite stand in for the person
//! (`KROWK_TEST_UNATTENDED_DEVICE_APPROVAL=1`, and the code on stdin); a
//! release build has no such path.

use super::Ctx;
use super::sync::{device_name, keyed_client, keystore, printable};
use crate::output::Format;
use krowk_api::sync::{ListPost, Pairing, PairingRequestState, PairingStep};
use krowk_api::{fail, Client, Error};
use krowk_client::device_chain::{Action, Chain, Change, Entry, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceId, DeviceKey, DeviceSigner, SigningKey};
use krowk_client::keystore::{Keystore, Slot};
use krowk_client::pairing::{self, Binding, NewDevice, PairA, PairingCode, PeerKind};
use krowk_client::pairing_join::{self, Field, JoinFailed, Mailbox};
use krowk_client::pairing_payload;
use krowk_client::user_key::UserKeys;
use serde_json::json;
use std::time::{Duration, Instant};

/// How often A reads the pairing while it waits for B: a person is walking
/// between two machines. B waits by `pairing_join::Timing::default()`, the
/// same 500 ms and ten minutes.
const POLL: Duration = Duration::from_millis(500);
/// How long A waits for B: the registry's ten minutes.
const WAIT: Duration = Duration::from_secs(10 * 60);

fn unattended(ctx: &Ctx) -> bool {
    cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1"
}

fn terminal(ctx: &Ctx, refused: &str) -> Result<(), Error> {
    if unattended(ctx) || (ctx.io.stdin_tty && ctx.io.err_tty) {
        return Ok(());
    }
    Err(fail("confirmation_required", refused.to_string()))
}

fn now() -> u64 {
    jiff::Timestamp::now().as_second().max(0) as u64
}

/// What a person calls this machine's OS on the prompt A shows.
fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn clip(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim().to_string()
}

fn signer(store: &Keystore, device: &DeviceKey) -> Result<(SigningKey, std::sync::Arc<dyn krowk_api::client::RequestSigner>), Error> {
    let signing = store.signing_key().map_err(|e| fail("sync_setup_failed", e))?;
    let copy = SigningKey::from_secret(&*signing.secret_bytes()).map_err(|e| fail("sync_setup_failed", e.0))?;
    Ok((signing, DeviceSigner::new(device.id(), copy).shared()))
}

fn entries(list: &krowk_api::sync::DeviceList) -> Result<Vec<SignedEntry>, Error> {
    let bad = || fail("device_list_invalid", "the registry served a device list entry that is not hex — nothing was changed");
    list.entries
        .iter()
        .map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).ok_or_else(bad)?, &e2e::unhex(&e.signatures).ok_or_else(bad)?).map_err(|e| fail("device_list_invalid", e.0)))
        .collect()
}

fn hex(b: &[u8]) -> String {
    e2e::hex(b)
}

/// Whether the person pressed ^C while `devices add` waits: the wait stops,
/// and the pairing is ended on the registry rather than left open for its
/// ten minutes.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// ^C noted rather than fatal while it lives, and fatal again after.
struct Interrupts;

impl Interrupts {
    fn catch() -> Interrupts {
        INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        Interrupts
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        // SAFETY: restoring the default disposition.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// A reads the pairing until `field` has arrived. A pairing that ended, or
/// the window closing, is the failure `failed` names. A's side holds no
/// guess, so a read the network lost is only a read, asked again until the
/// window closes. B's wait, which ends at any failure, is
/// `krowk_client::pairing_join`'s.
fn wait_for(client: &Client, id: &str, field: Field, failed: &dyn Fn(Option<Error>) -> Error) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + WAIT;
    loop {
        let p = match client.show_pairing(id) {
            Ok(p) => p,
            Err(e) if e.status == 0 && Instant::now() <= deadline && !interrupted() => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(e) => return Err(failed(Some(e))),
        };
        if let Some(b) = field.blob(&p) {
            return Ok(b);
        }
        if p.state == "dead" || Instant::now() > deadline || interrupted() {
            return Err(failed(None));
        }
        std::thread::sleep(POLL);
    }
}

// ------------------------------------------------------------ A: devices add

const ADD_OFF_TERMINAL: &str = "`krowk machines add` hands your key to another machine once a person has said yes to it, so it needs a person at a terminal — run it in one";

/// A: shows a code, waits for the new machine, checks its confirmation,
/// asks the person and sends the reply. Once the reply, which carries the
/// user key, has gone out, posts the new machine's `add` entry and the user
/// key wrapped to it whether or not the machine acknowledges, so a machine
/// that may hold the key is listed and can be removed; with no ack it ends
/// the pairing and fails `pairing_unconfirmed`, the machine listed. A
/// pairing that ends before the reply posts nothing.
pub(super) fn add(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(fail("bad_argument", "`krowk machines add` takes no arguments — it shows a code for the new machine to type"));
    }
    terminal(ctx, ADD_OFF_TERMINAL)?;
    // ^C from here on ends the pairing rather than leaving it open.
    let interrupts = Interrupts::catch();
    let store = keystore(ctx)?;
    let setup_first = || fail("no_user_key", "this machine is not on your device list yet — set sync up with `krowk sync init`, or add it from another device with `krowk sync join`");
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?.ok_or_else(setup_first)?;
    if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    if store.device_list().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    let (signing, shared) = signer(&store, &device)?;
    let client = keyed_client(ctx, "`krowk machines add`")?.signed_by(shared);

    let (served_entries, chain, keys) = current_list(&store, &client, &device)?;
    if !chain.devices().iter().any(|d| d.id() == device.id() && d.kind == Kind::Device) {
        return Err(fail("device_removed", "this machine is not on your device list — it was removed; pair it again with `krowk sync join`"));
    }

    let user = client.verify_key()?.user_id;
    let a = PairA::new(PeerKind::SamePersonDevice, user.clone(), device.id()).map_err(|e| fail("pairing_failed", e.0))?;
    let opened = client.open_pairing()?;
    let id = opened.id.clone();
    // Up to the reply, anything that goes wrong ends the pairing on the
    // registry too, so the new machine hears it at once, and adds nothing.
    let sealed = match seal_reply(ctx, &client, &id, a, (&chain, served_entries), &keys, (&device, &signing)) {
        Ok(sealed) => sealed,
        Err(e) => {
            let _ = client.end_pairing();
            return Err(e);
        }
    };
    // The reply went out with the user key in it: from here on the entry is
    // posted whatever B does, so a machine that may hold the key is listed.
    let acked = wait_for(&client, &id, Field::JoinerAck, &a_failed).and_then(|ack| sealed.await_ack.receive_ack(&ack).map_err(|_| a_failed(None)));
    if acked.is_err() {
        let _ = client.end_pairing();
    }
    let posted = post_add(&client, &sealed.batch);
    drop(interrupts);
    let (name, os) = (sealed.name, sealed.os);
    match (acked, posted) {
        (Ok(_), Ok(())) => {}
        (Err(_), Ok(())) => {
            let _ = store.save_device_list(&sealed.entries);
            return Err(fail(
                "pairing_unconfirmed",
                format!("'{name}' was sent your key but never confirmed it. It is on your devices so you can take it off and rotate your key away from it: `krowk machines remove '{name}'`"),
            ));
        }
        (_, Err(e)) => {
            return Err(fail(
                "pairing_unconfirmed",
                format!("'{name}' was sent your key, but it could not be added to your devices ({}) — it may hold your key without being listed. Check `krowk machines`; rotate your key with the next `krowk machines remove`", e.code()),
            ));
        }
    }
    store.save_device_list(&sealed.entries).map_err(|e| fail("sync_setup_failed", e))?;
    let seq = sealed.next.head().seq;
    let summary = format!("Added. '{name}' ({os}) syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "name": name, "os": os, "seq": seq }), summary)
}

/// What the host daemon opens a pairing for a new machine with: this
/// machine's keys and its list, read as `krowk machines add` reads them — a
/// machine not on the list has nothing to add anyone with, and the list is
/// the registry's current one, verified from this machine's pin.
#[cfg(unix)]
pub(super) fn introducer(ctx: &Ctx) -> Result<krowk_harness::sync::introduce::Introducer, Error> {
    let store = keystore(ctx)?;
    let setup_first = || fail("no_user_key", "this machine is not on your device list yet — set sync up with `krowk sync init`, or add it from another device with `krowk sync join`");
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?.ok_or_else(setup_first)?;
    if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_none() || store.device_list().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    let (signing, shared) = signer(&store, &device)?;
    let client = keyed_client(ctx, "a pairing for a new machine")?.signed_by(shared);
    let (entries, chain, keys) = current_list(&store, &client, &device)?;
    // The list this machine keeps as its pin, taken up to the new entry once
    // it is posted, as `devices add` does.
    let keep: krowk_harness::sync::introduce::Keep = Box::new(move |entries| store.save_device_list(entries).map(|_| ()));
    Ok(krowk_harness::sync::introduce::Introducer { api: std::sync::Arc::new(client), keys, chain, entries, device: device.id(), signing, keep: Some(keep) })
}

/// The device list as the registry has it now, kept only if it extends
/// this machine's pin (a shorter, forked or started-over list is refused),
/// and this machine's user keys caught up to the generation it leaves
/// current. What a device wraps to or seals under is this, never the list
/// it kept from before, which a removal it slept through has left behind:
/// a registry that cannot be reached is a refusal, not a fall back to it.
pub(super) fn current_list(store: &Keystore, client: &Client, device: &DeviceKey) -> Result<(Vec<SignedEntry>, Chain, UserKeys), Error> {
    let served = client.device_list_all().map_err(|mut e| {
        e.body.insert("fix".into(), json!("the registry could not be asked for your device list, and nothing is sealed under a list that may be stale — try again when it answers"));
        e
    })?;
    let served = entries(&served)?;
    let chain = store.save_device_list(&served).map_err(|e| fail("device_list_refused", e))?;
    let keys = caught_up(store, client, &chain, device)?;
    Ok((served, chain, keys))
}

/// This device's user keys at the generation the verified chain leaves
/// current. A device a rotation left behind takes up the newer generation
/// here, as every device does (`UserKeys::adopt`): its wrap must open to the
/// id the chain names, and lead back down to the key it holds. Every older
/// generation is then held to the chain's ids too (`verified_by`).
fn caught_up(store: &Keystore, client: &Client, chain: &Chain, device: &DeviceKey) -> Result<UserKeys, Error> {
    let setup = |e: String| fail("sync_setup_failed", e);
    let held = store.user_keys().map_err(setup)?.ok_or_else(|| fail("no_user_key", "this machine holds no user key — add it from another device with `krowk sync join`"))?;
    if held.newest().generation() < chain.generation() {
        let behind = || fail("user_key_behind", "the registry holds no wrap of your current user key for this machine — it may have been removed; check `krowk machines`");
        let served = client.user_key()?;
        let g = chain.generation();
        let wrap = served.wraps.iter().find(|w| w.generation == g).and_then(|w| e2e::unhex(&w.wrapped_key)).ok_or_else(behind)?;
        let links = served.generations.iter().filter(|x| !x.wrapped_previous.is_empty()).filter_map(|x| e2e::unhex(&x.wrapped_previous));
        let adopted = UserKeys::adopt(held.newest(), &wrap, g, chain.key_id(), device, links).map_err(|e| fail("user_key_refused", e.0))?;
        store.save_user_keys(&adopted).map_err(setup)?;
    }
    let keys = store.user_keys().map_err(setup)?.ok_or_else(|| setup("the user keys went missing".into()))?;
    keys.verified_by(chain).map_err(|e| fail("user_key_refused", e.0))
}

fn a_failed(e: Option<Error>) -> Error {
    let why = e.map(|e| format!(" ({})", e.code())).unwrap_or_default();
    fail("pairing_failed", format!("the pairing ended before the new device was added{why} — nothing was added; run `krowk machines add` again for a new code"))
}

/// One of A's steps, sent once. When its answer is lost, the pairing is
/// read back: the step landed if it holds what was sent, and the pairing is
/// over otherwise — sending it again would be out of turn.
fn a_step(client: &Client, id: &str, step: PairingStep, field: Field, message: &[u8]) -> Result<(), Error> {
    match client.pairing_step(step, message) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => match client.show_pairing(id) {
            Ok(p) if field.blob(&p).as_deref() == Some(message) => Ok(()),
            _ => Err(a_failed(Some(e))),
        },
        Err(e) => Err(a_failed(Some(e))),
    }
}

/// The reply, sent once. It carries the user key, so when its answer is
/// lost and the read-back fails too, nobody can say it did not land: it is
/// taken as sent, and A goes on to the ack and the post, which list a
/// machine that may hold the key. Only a refusal, or a read-back that shows
/// it did not land, ends the pairing with nothing added.
fn send_reply(client: &Client, id: &str, reply: &[u8]) -> Result<(), Error> {
    match client.pairing_step(PairingStep::Reply, reply) {
        Ok(_) => Ok(()),
        // Lost on the way back, or a gateway's answer, which may come after
        // the registry stored it: either way it may have landed.
        Err(e) if matches!(e.status, 0 | 502..=504) => match client.show_pairing(id) {
            Ok(p) if Field::SealedReply.blob(&p).as_deref() != Some(reply) => Err(a_failed(Some(e))),
            _ => Ok(()),
        },
        Err(e) => Err(a_failed(Some(e))),
    }
}

/// The list A verified: the chain, and its entries as served.
type Verified<'a> = (&'a Chain, Vec<SignedEntry>);

/// A's side once the reply is out: what it posts, the chain it leaves,
/// and the wait for B's ack.
struct Sealed {
    batch: krowk_client::device_chain::Batch,
    /// The whole list with the new entry on its end, to keep once posted.
    entries: Vec<SignedEntry>,
    next: Chain,
    await_ack: krowk_client::pairing::AwaitAck,
    name: String,
    os: String,
}

/// From the hello to the reply: B's confirmation checked before the person
/// is asked anything, then the reply sealed and sent.
fn seal_reply(ctx: &mut Ctx, client: &Client, id: &str, a: PairA, (chain, mut all): Verified<'_>, keys: &UserKeys, (device, signing): (&DeviceKey, &SigningKey)) -> Result<Sealed, Error> {
    let _ = writeln!(ctx.io.stderr, "Pair a new device\n  Code: {}          valid 10 minutes, once\nOn the new device run:  krowk sync join\nWaiting…", a.code());
    let _ = ctx.io.stderr.flush();
    let hello = wait_for(client, id, Field::JoinerMessage, &a_failed)?;
    let (await_confirm, spake) = a.receive_hello(&hello).map_err(|_| a_failed(None))?;
    a_step(client, id, PairingStep::Answer, Field::InitiatorMessage, &spake)?;
    let confirmation = wait_for(client, id, Field::JoinerConfirmation, &a_failed)?;
    // A wrong code fails here: the pairing ends, and the person is asked
    // nothing.
    let confirmed = await_confirm.receive_confirm(&confirmation).map_err(|_| {
        fail("pairing_failed", "the new device's confirmation did not check — the code was wrong, or someone else tried it; nothing was added. Run `krowk machines add` again for a new code")
    })?;
    let new = confirmed.device().clone();
    let (name, os) = (printable(&new.name), printable(&new.os));
    // Before the person is asked: a name that reads like a listed device's
    // is the new machine's to change, and the pairing ends here.
    if let Some(d) = chain.devices().iter().find(|d| krowk_client::device_chain::confusable_names(&d.name, &new.name)) {
        return Err(fail("device_name_taken", format!("the new device calls itself '{name}', which reads like '{}' already on your devices — nothing was added. On the new device run `krowk sync join --name <another name>`, and here `krowk machines add` again", printable(&d.name))));
    }
    ask(ctx, &format!("Add '{name}' ({os}) to your devices?"))?;
    if interrupted() {
        return Err(a_failed(None));
    }
    let subject = Subject { kind: Kind::Device, name: new.name.clone(), os: new.os.clone(), device: new.device, signing: new.signing };
    let (next, batch) = chain.batch(keys.newest(), vec![Change::Add(subject)], device.id(), signing, now()).map_err(|e| fail("device_list_refused", e.0))?;
    let wrapped = batch.wraps.iter().find(|(d, _)| *d == new.id()).map(|(_, w)| w.clone()).ok_or_else(|| fail("device_list_refused", "the new device's entry carried no wrap for it"))?;
    all.extend(batch.entries.iter().cloned());
    let bytes = pairing_payload::seal(&all, keys, next.generation(), &wrapped, &signing.public());
    let (await_ack, reply) = confirmed.approve(&bytes).map_err(|_| a_failed(None))?;
    send_reply(client, id, &reply)?;
    Ok(Sealed { batch, entries: all, next, await_ack, name, os })
}

/// Posts the new device's entry and wrap, once. When the answer is lost the
/// list is read back: the post landed if the entry is on it.
fn post_add(client: &Client, batch: &krowk_client::device_chain::Batch) -> Result<(), Error> {
    let post = ListPost {
        entries: batch.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    match client.append_device_list(&post) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => {
            let first = hex(&batch.entries[0].bytes);
            match client.device_list_all() {
                Ok(list) if list.entries.iter().any(|x| x.entry == first) => Ok(()),
                _ => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// The one `[Y/n]` naming the new device. Off a terminal only a debug
/// build's test stand-in answers, and it says yes.
fn ask(ctx: &mut Ctx, question: &str) -> Result<(), Error> {
    if unattended(ctx) {
        let _ = writeln!(ctx.io.stderr, "{question} [Y/n] y");
        return Ok(());
    }
    match inquire::Confirm::new(question).with_default(true).prompt() {
        Ok(true) => Ok(()),
        _ => Err(fail("selection_cancelled", "not added — the new device was told, and nothing reached your device list")),
    }
}

// ------------------------------------------- A: the host daemon's offer

/// What the host daemon opened for a new machine that signed in
/// (R-EASE-5), shown by `krowk machines`: its code, and once the code was
/// typed and its confirmation checked, the question `devices add` asks. On
/// a terminal the question is asked here and the answer sent to the daemon;
/// off one only the code is shown, since adding a machine hands it every
/// session and takes a person. Nothing is shown when the daemon holds no
/// offer, or none runs.
#[cfg(unix)]
pub(super) fn show_offer(ctx: &mut Ctx) {
    use krowk_client::protocol::OfferStage;
    let ask_here = unattended(ctx) || (ctx.io.tty && ctx.io.stdin_tty && ctx.io.err_tty);
    let deadline = Instant::now() + WAIT;
    let mut said_code = false;
    loop {
        let Ok(Some(offer)) = krowk_harness::daemon::pairing_offer(ctx.io.env, super::VERSION) else { return };
        if Instant::now() > deadline {
            return;
        }
        match offer.stage {
            OfferStage::Waiting => {
                // The code goes only to a person at a terminal: an agent that
                // runs `krowk machines` is not shown it.
                if !ask_here {
                    let _ = writeln!(ctx.io.stdout, "A new machine signed in to your account. Run krowk machines in a terminal to see its code.");
                    return;
                }
                let _ = writeln!(ctx.io.stdout, "A new machine signed in to your account. Its code: {}", printable(&offer.code));
                said_code = true;
                // The person walks to the new machine and types it: the
                // question comes when its checks have passed.
                loop {
                    std::thread::sleep(POLL);
                    match krowk_harness::daemon::pairing_offer(ctx.io.env, super::VERSION) {
                        Ok(Some(o)) if o.stage == OfferStage::Waiting && Instant::now() < deadline => {}
                        _ => break,
                    }
                }
            }
            OfferStage::Confirm => {
                if !ask_here {
                    let _ = writeln!(ctx.io.stdout, "A new machine is waiting to be added. Run krowk machines in a terminal to answer.");
                    return;
                }
                let question = format!("Add '{}' ({}) to your devices? [Y/n]", printable(&offer.name), printable(&offer.os));
                let yes = matches!(super::chain::ask_key(ctx, &question, &[]).as_str(), "" | "y" | "yes");
                if let Err(e) = krowk_harness::daemon::pairing_answer(ctx.io.env, super::VERSION, &offer.pairing, yes) {
                    let _ = writeln!(ctx.io.stderr, "{}", printable(&e.message));
                    return;
                }
                // The answer is carried out on the daemon's side: its
                // outcome follows.
                for _ in 0..40 {
                    std::thread::sleep(POLL);
                    if let Ok(Some(o)) = krowk_harness::daemon::pairing_offer(ctx.io.env, super::VERSION)
                        && matches!(o.stage, OfferStage::Added | OfferStage::Failed)
                    {
                        let _ = writeln!(ctx.io.stdout, "{}", printable(&o.note));
                        return;
                    }
                }
                return;
            }
            OfferStage::Added | OfferStage::Failed => {
                // Said after the question, above; a later look says how it
                // ended, for a minute.
                if !said_code {
                    let _ = writeln!(ctx.io.stdout, "{}", printable(&offer.note));
                }
                return;
            }
        }
    }
}

/// No host daemon where there is no unix socket: nothing to show.
#[cfg(not(unix))]
pub(super) fn show_offer(_: &mut Ctx) {}

// --------------------------------------------------------------- B: sync join

const JOIN_OFF_TERMINAL: &str = "`krowk sync join` takes a pairing code typed by a person, never an argument or a pipe, so it needs a person at a terminal — run it in one";

/// The failure every step on B ends in: the code is used up.
fn b_failed(from: &str) -> Error {
    fail(
        "pairing_failed",
        format!("the pairing failed, and a code works once — run `krowk` here again for a new one from '{from}' or another of your machines. Nothing was kept"),
    )
}

/// The code, typed at a prompt: never an argument, so it stays out of shell
/// history and process lists. A debug build's tests type it on stdin.
fn typed_code(ctx: &Ctx) -> Result<PairingCode, Error> {
    let typed = if unattended(ctx) {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|_| fail("selection_cancelled", "no pairing code was entered"))?;
        krowk_client::Zeroizing::new(line)
    } else {
        krowk_client::Zeroizing::new(inquire::Text::new("Code:").prompt().map_err(|_| fail("selection_cancelled", "no pairing code was entered"))?)
    };
    // Parsed once: the string is dropped here, and the code it made is
    // consumed by the one pairing it starts.
    PairingCode::parse(&typed).ok_or_else(|| fail("bad_pairing_code", "that is not a pairing code — it is the eight characters your other machine shows, like K7QF-9M3X; nothing was sent"))
}

/// B: signs in if it has to, finds the person's one open pairing, takes
/// the code at a prompt, and keeps the keys and the pin only once the
/// chain A sealed adds exactly this machine.
pub(super) fn join(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(fail("bad_argument", "`krowk sync join` takes the code at its prompt, never as an argument — one given here is in your shell history now, so get a new code with `krowk machines add` and type it when asked"));
    }
    terminal(ctx, JOIN_OFF_TERMINAL)?;
    let mut client = super::agent::new_client(ctx)?;
    if !client.authenticated() {
        super::auth::login(ctx, &[])?;
        client = keyed_client(ctx, "`krowk sync join`")?;
    }
    join_with(ctx, client, true)
}

/// The first-run flow's hand-off for an account that already has machines
/// (`first_run::add_a_machine`): the same join, as the machine just signed
/// in, ending at "Joined as …". The always-on offer is not made here — the
/// setup flow asks it after the model step.
pub(super) fn join_new_machine(ctx: &mut Ctx, client: Client) -> Result<(), Error> {
    terminal(ctx, JOIN_OFF_TERMINAL)?;
    join_with(ctx, client, false)
}

fn join_with(ctx: &mut Ctx, client: Client, offers: bool) -> Result<(), Error> {
    let key = client.verify_key()?;
    let who = if !key.email.is_empty() { &key.email } else if key.workspace_name.is_empty() { &key.workspace } else { &key.workspace_name };
    let _ = writeln!(ctx.io.stderr, "Signed in as {}", printable(who));
    let store = keystore(ctx)?;
    let mut device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
    let (mut signing, mut shared) = signer(&store, &device)?;
    // Nothing here is forgotten on the registry's word: a user key left by
    // a pairing A never posted is replaced only once A's own chain says so.
    match listed(&client, &device.id())? {
        Listing::Active if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_some() => {
            return Err(fail("already_joined", "this machine is already on your device list — there is nothing to join"));
        }
        // Its keys are in the list's history, so no list takes them again:
        // the person is offered to start over with new ones, the old moved
        // aside.
        Listing::Removed => {
            let old = device.id();
            start_over(ctx, &store)?;
            device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
            if device.id() == old {
                return Err(fail("sync_setup_failed", "this machine's old keys could not be moved aside, so it has no new ones — move them aside by hand, then run `krowk sync join` again"));
            }
            (signing, shared) = signer(&store, &device)?;
        }
        _ => {}
    }
    let (open, code) = open_pairing_and_code(ctx, &client)?;
    let a_device = DeviceId::parse(&open.initiator_device.id).ok_or_else(|| fail("malformed_response", "the pairing names no device id"))?;
    // Only the paired device's id is bound by the pairing; its name is the
    // registry's, shown as a hint.
    let from = printable(&open.initiator_device.name);
    let _ = writeln!(ctx.io.stderr, "Waiting for {from}…");
    let _ = ctx.io.stderr.flush();
    let me = NewDevice { device: device.public(), signing: signing.public(), name: clip(&device_name(ctx), pairing::MAX_NAME), os: os_name().into() };
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: key.user_id.clone(), a_device, b_device: device.id() };
    // One client for the whole join, signed by this device for the claim;
    // B's steps still go with the key alone (`pairing_step` signs only A's).
    let mailbox = ClientMailbox { client: client.signed_by(shared), id: open.id.clone() };
    let joined = pairing_join::join(&mailbox, &store, &device, binding, code, me, &pairing_join::Timing::default());
    let pairing_join::Joined { seq, name } = match joined {
        Ok(joined) => joined,
        Err(JoinFailed::CodeUsedUp) => return Err(b_failed(&from)),
        Err(JoinFailed::NotAdded) => {
            return Err(fail(
                "pairing_failed",
                format!("'{from}' did not add this machine to your device list, or the registry could not be reached to see it — the keys were forgotten and the code is used up; run `krowk` here again for a new code"),
            ));
        }
    };
    let summary = format!("Joined as '{name}'. This machine syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
    }
    if !offers {
        return Ok(());
    }
    let service = offer_service(ctx);
    let _ = writeln!(ctx.io.stderr, "{TAKES_WORK}");
    if ctx.format == Format::Human {
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "device": device.id().to_string(), "name": name, "seq": seq, "joined": true, "hostService": service }), summary)
}

const NO_HOST: &str = "None of your machines is online to show a code. Run krowk machines add on one of them.";

/// A pairing a person already has open (a `krowk machines add` or `krowk
/// devices add` on another machine) is joined as it stands. With none, the
/// new machine asks the person's hosts for one and waits for it, then asks
/// for the code the host shows.
fn open_pairing_and_code(ctx: &mut Ctx, client: &Client) -> Result<(Pairing, PairingCode), Error> {
    let open = match client.find_open_pairing() {
        Ok(open) => open,
        Err(e) if e.code() == "no_pairing" => ask_hosts(ctx, client)?,
        Err(e) => return Err(e),
    };
    let _ = writeln!(ctx.io.stderr, "Type the code shown on your other machine: ____-____");
    let code = typed_code(ctx)?;
    Ok((open, code))
}

/// How long the new machine waits for a host to open a pairing before it
/// says none is online: three of a host's refreshes (`announce::EVERY`) and a
/// breath. A debug build's tests shorten it.
fn no_host_after(ctx: &Ctx) -> Duration {
    let tested = ctx.env("KROWK_TEST_NO_HOST_SECS").parse::<u64>().ok().filter(|_| cfg!(debug_assertions));
    Duration::from_secs(tested.unwrap_or(40))
}

/// The new machine asks the person's hosts, and waits for the pairing one
/// of them opens.
fn ask_hosts(ctx: &mut Ctx, client: &Client) -> Result<Pairing, Error> {
    let name = clip(&device_name(ctx), pairing::MAX_NAME);
    let request = match client.request_pairing(&name, os_name()) {
        Ok(r) => r,
        // One already recorded (the answer to an earlier try was lost, or
        // this very machine asked a moment ago): read it.
        Err(e) if e.code() == "pairing_request_pending" => client.pairing_request()?,
        Err(e) if e.code() == "no_device_list" => {
            return Err(fail("no_device_list", "your account has no devices yet, so there is no machine to show a code — set this one up first with `krowk`"));
        }
        Err(e) => return Err(e),
    };
    let _ = writeln!(ctx.io.stderr, "Asking your other machines to show a code… `krowk machines` on one of them shows it.");
    let _ = ctx.io.stderr.flush();
    let poll = if unattended(ctx) { Duration::from_millis(100) } else { Duration::from_secs(request.poll_interval.clamp(1, 5)) };
    // A host online refreshes its presence every few seconds, and a request
    // reaches it in that refresh: a few refreshes with no pairing opened
    // means none is. (The new machine is no device yet, so it cannot read
    // which are online.)
    let deadline = Instant::now() + no_host_after(ctx);
    loop {
        // The state is a hint that stays after its pairing ends: the pairing
        // itself says whether there is one to join.
        match client.find_open_pairing() {
            Ok(open) => return Ok(open),
            Err(e) if e.code() == "no_pairing" => {}
            Err(e) if e.status == 0 => {}
            Err(e) => return Err(e),
        }
        if client.pairing_request().is_ok_and(|r| r.state == PairingRequestState::Expired) || Instant::now() > deadline {
            return Err(fail("no_host_online", NO_HOST));
        }
        std::thread::sleep(poll);
    }
}

/// The offer to start over as a new machine, for a machine whose keys were
/// removed from the list: yes moves the old device and signing keys aside
/// (each renamed beside itself, never deleted), and new ones are made.
fn start_over(ctx: &mut Ctx, store: &Keystore) -> Result<(), Error> {
    terminal(ctx, JOIN_OFF_TERMINAL)?;
    let question = "This machine was removed from your devices. Start it over as a new machine?";
    if unattended(ctx) {
        let _ = writeln!(ctx.io.stderr, "{question} [Y/n] y");
    } else {
        match inquire::Confirm::new(question).with_default(true).prompt() {
            Ok(true) => {}
            _ => return Err(fail("selection_cancelled", "this machine stays as it was — it can join only with new device keys")),
        }
    }
    let stamp = now();
    for slot in [Slot::Device, Slot::Signing] {
        let from = std::path::PathBuf::from(store.describe(slot));
        let mut to = from.clone().into_os_string();
        to.push(format!(".removed-{stamp}"));
        match std::fs::rename(&from, std::path::PathBuf::from(&to)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail("sync_setup_failed", format!("{} could not be moved aside: {e}", from.display()))),
        }
    }
    let _ = writeln!(ctx.io.stderr, "The old keys are moved aside (…removed-{stamp}).");
    Ok(())
}

/// What a join ends saying (R-ALLOC-13): acceptance is the default.
const TAKES_WORK: &str = "This machine now takes work from your other machines in trusted repositories. Turn it off with krowk machines accept off.";

/// The always-on host service a machine needs to take work, offered at the
/// end of a join, yes by default: a laptop joined only to follow declines.
/// The join is done whatever happens here, so a service that will not start
/// is said, never an error. Whether it was enabled.
#[cfg(unix)]
fn offer_service(ctx: &mut Ctx) -> bool {
    let question = "Run the always-on host service, so this machine can take work from your other machines?";
    let yes = if unattended(ctx) {
        let mut line = String::new();
        // No line at all (stdin closed) is no: a test stand-in never
        // installs a service it was not asked to.
        let read = std::io::stdin().read_line(&mut line).unwrap_or(0);
        let yes = read > 0 && !matches!(line.trim().to_lowercase().as_str(), "n" | "no");
        let _ = writeln!(ctx.io.stderr, "{question} [Y/n] {}", if yes { "y" } else { "n" });
        yes
    } else {
        inquire::Confirm::new(question).with_default(true).prompt().unwrap_or(false)
    };
    if !yes {
        let _ = writeln!(ctx.io.stderr, "Not enabled. `krowk machines always-on on` does it later.");
        return false;
    }
    match super::host::set_service(ctx, true) {
        Ok((file, _)) => {
            let _ = writeln!(ctx.io.stderr, "The host daemon runs as a service: {}", file.display());
            true
        }
        Err(e) => {
            let _ = writeln!(ctx.io.stderr, "The host service did not start: {e} Run `krowk machines always-on on` to try again.");
            false
        }
    }
}

#[cfg(not(unix))]
fn offer_service(_: &mut Ctx) -> bool {
    false
}

#[derive(PartialEq, Eq)]
enum Listing {
    Never,
    Active,
    Removed,
}

/// Where `device` stands on the person's device list as the registry serves
/// it, replaying its adds and removes: only ever a reason to refuse, so the
/// registry's word is enough, and it never makes this machine forget.
fn listed(client: &Client, device: &DeviceId) -> Result<Listing, Error> {
    let list = client.device_list_all()?;
    let mut at = Listing::Never;
    for e in entries(&list)?.iter().filter_map(|e| Entry::decode(&e.bytes).ok()) {
        if e.subjects.iter().any(|s| s.id() == *device) {
            at = match e.action {
                Action::Add => Listing::Active,
                Action::Remove => Listing::Removed,
                _ => at,
            };
        }
    }
    Ok(at)
}

/// The registry's mailbox as `krowk sync join` reaches it: `client` is
/// the keyed client, signed by this device, and `id` the pairing it is in.
struct ClientMailbox {
    client: Client,
    id: String,
}

impl Mailbox for ClientMailbox {
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), Error> {
        self.client.pairing_step(step, message).map(|_| ())
    }

    fn read(&self) -> Result<Pairing, Error> {
        self.client.show_pairing(&self.id)
    }

    fn end(&self) {
        let _ = self.client.end_pairing();
    }

    fn claim_device(&self) -> Result<(), Error> {
        self.client.claim_key_device().map(|_| ())
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}
